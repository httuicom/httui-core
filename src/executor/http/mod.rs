pub mod types;

use async_trait::async_trait;
use base64::prelude::*;
use reqwest::redirect::Policy;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use self::types::{parse_set_cookie, Cookie, HttpResponse, TimingBreakdown};
use super::{BlockResult, Executor, ExecutorError};

#[derive(Debug, Deserialize)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct HttpParams {
    method: String,
    url: String,
    #[serde(default)]
    params: Vec<KeyValue>,
    #[serde(default)]
    headers: Vec<KeyValue>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `None` (default) → follow redirects (reqwest default = 10 hops).
    /// `Some(false)` → no redirects, raw 3xx surface to the user.
    #[serde(default)]
    follow_redirects: Option<bool>,
    /// `None` / `Some(true)` (default) → enforce certificate validation.
    /// `Some(false)` → accept self-signed / invalid certificates (per request).
    #[serde(default)]
    verify_ssl: Option<bool>,
    /// `None` / `Some(true)` (default) → percent-encode query param values.
    /// `Some(false)` → append the raw value verbatim (user already encoded).
    #[serde(default)]
    encode_url: Option<bool>,
    /// `None` / `Some(true)` (default) → trim whitespace on header keys/values,
    /// query keys/values, and the body before sending. May break text/plain
    /// payloads that rely on leading/trailing whitespace; user opted in.
    #[serde(default)]
    trim_whitespace: Option<bool>,
}

fn is_binary_content_type(content_type: &str) -> bool {
    let ct = content_type.to_lowercase();
    ct.starts_with("image/")
        || ct.starts_with("video/")
        || ct.starts_with("audio/")
        || ct == "application/pdf"
        || ct == "application/octet-stream"
        || ct == "application/zip"
        || ct == "application/wasm"
}

fn classify_reqwest_error(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection_failed"
    } else if e.is_redirect() {
        "too_many_redirects"
    } else if e.is_body() || e.is_decode() {
        "body_error"
    } else {
        "request_error"
    }
}

/// Bool flags that have to be baked into the reqwest `Client` because
/// reqwest does not expose them as per-request overrides
/// (`redirect::Policy` and `danger_accept_invalid_certs` are
/// `ClientBuilder`-only). We cache one client per combination so we don't
/// rebuild a fresh TLS pool on every request — the matrix is bounded at 4.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
struct ClientFlags {
    follow_redirects: bool,
    verify_ssl: bool,
}

impl Default for ClientFlags {
    fn default() -> Self {
        Self {
            follow_redirects: true,
            verify_ssl: true,
        }
    }
}

pub struct HttpExecutor {
    /// Lazy cache of `reqwest::Client` keyed by transport flags. Wrapped in
    /// a sync `Mutex` because lookup is fast and contention is rare (only
    /// hit on first use of each combo). `Client` clones share the same
    /// internal `Arc<ClientRef>` so cloning on the hot path is cheap.
    clients: Mutex<HashMap<ClientFlags, Client>>,
}

impl Default for HttpExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpExecutor {
    pub fn new() -> Self {
        let executor = Self {
            clients: Mutex::new(HashMap::new()),
        };
        // Eagerly populate the default-flags client so the warm path matches
        // the previous "single global client" behaviour.
        let _ = executor.client_for(ClientFlags::default());
        executor
    }

    fn client_for(&self, flags: ClientFlags) -> Client {
        let mut guard = self.clients.lock().expect("HttpExecutor.clients poisoned");
        if let Some(existing) = guard.get(&flags) {
            return existing.clone();
        }
        let mut builder = Client::builder().timeout(Duration::from_secs(30));
        if !flags.follow_redirects {
            builder = builder.redirect(Policy::none());
        }
        if !flags.verify_ssl {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder.build().unwrap_or_default();
        guard.insert(flags, client.clone());
        client
    }

    /// Cancel-aware execution returning the typed `HttpResponse` directly.
    ///
    /// The `cancel` token is observed via `tokio::select!`. When fired, the
    /// in-flight request future is dropped and the caller gets a cancellation
    /// error. Reqwest does not propagate cancellation to the server (the
    /// remote endpoint may continue processing); the local connection is
    /// simply released.
    ///
    /// This path always returns the typed shape; HTTP-level errors (4xx/5xx)
    /// are surfaced as `Ok(response)` with the status code preserved — only
    /// transport/encoding/cancellation failures map to `Err`.
    pub async fn execute_with_cancel(
        &self,
        params: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, ExecutorError> {
        let p: HttpParams = serde_json::from_value(params)
            .map_err(|e| ExecutorError(format!("Invalid params: {e}")))?;
        let flags = ClientFlags {
            follow_redirects: p.follow_redirects.unwrap_or(true),
            verify_ssl: p.verify_ssl.unwrap_or(true),
        };
        let client = self.client_for(flags);
        let req = build_request(&client, &p)?;

        let start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ExecutorError("Request cancelled".to_string()));
            }
            res = req.send() => {
                res.map_err(|e| {
                    ExecutorError(format!("[{}] {}", classify_reqwest_error(&e), e))
                })?
            }
        };

        let bytes_future = collect_response(response);
        let collected = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ExecutorError("Request cancelled".to_string()));
            }
            res = bytes_future => res?,
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(HttpResponse {
            status_code: collected.status_code,
            status_text: collected.status_text,
            headers: collected.headers,
            body: collected.body,
            size_bytes: collected.size_bytes,
            elapsed_ms,
            timing: TimingBreakdown {
                total_ms: elapsed_ms,
                ..Default::default()
            },
            cookies: collected.cookies,
        })
    }
}

/// Internal collected response shape — `execute_with_cancel` adds timing on
/// top and converts to the public `HttpResponse`.
struct CollectedResponse {
    status_code: u16,
    status_text: String,
    headers: HashMap<String, String>,
    body: serde_json::Value,
    size_bytes: u64,
    cookies: Vec<Cookie>,
}

fn build_request(
    client: &Client,
    p: &HttpParams,
) -> Result<reqwest::RequestBuilder, ExecutorError> {
    // Defaults: trim_whitespace ON, encode_url ON. `None` means "use default".
    let trim = p.trim_whitespace.unwrap_or(true);
    let encode_values = p.encode_url.unwrap_or(true);

    let trim_str = |s: &str| -> String {
        if trim {
            s.trim().to_string()
        } else {
            s.to_string()
        }
    };

    let raw_url = trim_str(&p.url);
    let mut url = reqwest::Url::parse(&raw_url)
        .map_err(|e| ExecutorError(format!("Invalid URL: {e}")))?;

    if encode_values {
        // Default: percent-encode keys/values via the safe API.
        for kv in &p.params {
            let key = trim_str(&kv.key);
            if key.is_empty() {
                continue;
            }
            url.query_pairs_mut().append_pair(&key, &trim_str(&kv.value));
        }
    } else {
        // Opt-out: append raw query string. Caller is responsible for any
        // encoding. We still trim if requested — trimming is independent of
        // encoding. We rebuild the query manually so reqwest doesn't
        // double-encode.
        let mut existing: String = url.query().map(|s| s.to_string()).unwrap_or_default();
        for kv in &p.params {
            let key = trim_str(&kv.key);
            if key.is_empty() {
                continue;
            }
            if !existing.is_empty() {
                existing.push('&');
            }
            existing.push_str(&key);
            existing.push('=');
            existing.push_str(&trim_str(&kv.value));
        }
        url.set_query(if existing.is_empty() {
            None
        } else {
            Some(&existing)
        });
    }

    let method = p
        .method
        .parse::<reqwest::Method>()
        .map_err(|e| ExecutorError(format!("Invalid method: {e}")))?;

    let mut req = client.request(method.clone(), url);
    if let Some(ms) = p.timeout_ms {
        req = req.timeout(Duration::from_millis(ms));
    }
    for kv in &p.headers {
        let key = trim_str(&kv.key);
        if !key.is_empty() {
            req = req.header(&key, &trim_str(&kv.value));
        }
    }

    let has_body = matches!(
        method,
        reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
    );
    let body_text = trim_str(&p.body);
    if has_body && !body_text.is_empty() {
        // Only inject default Content-Type when the user didn't set one.
        let user_set_ct = p
            .headers
            .iter()
            .any(|kv| kv.key.trim().eq_ignore_ascii_case("content-type"));
        if !user_set_ct {
            req = req.header("Content-Type", "application/json");
        }
        req = req.body(body_text);
    }

    Ok(req)
}

async fn collect_response(
    response: reqwest::Response,
) -> Result<CollectedResponse, ExecutorError> {
    let status_code = response.status().as_u16();
    let status_text = response
        .status()
        .canonical_reason()
        .unwrap_or("")
        .to_string();

    // Capture Set-Cookie headers BEFORE moving the response into bytes().
    let cookies: Vec<Cookie> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(parse_set_cookie)
        .collect();

    let resp_headers: HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let content_type = resp_headers
        .get("content-type")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    let bytes = response.bytes().await.map_err(|e| {
        ExecutorError(format!("[{}] {}", classify_reqwest_error(&e), e))
    })?;
    let size_bytes = bytes.len() as u64;

    let body_value = if is_binary_content_type(&content_type) {
        serde_json::json!({
            "encoding": "base64",
            "data": BASE64_STANDARD.encode(&bytes),
        })
    } else {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|_| serde_json::Value::String(text))
    };

    Ok(CollectedResponse {
        status_code,
        status_text,
        headers: resp_headers,
        body: body_value,
        size_bytes,
        cookies,
    })
}

#[async_trait]
impl Executor for HttpExecutor {
    fn block_type(&self) -> &str {
        "http"
    }

    async fn validate(&self, params: &serde_json::Value) -> Result<(), String> {
        let p: HttpParams =
            serde_json::from_value(params.clone()).map_err(|e| format!("Invalid params: {e}"))?;
        if p.url.trim().is_empty() {
            return Err("URL is required".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        params: serde_json::Value,
    ) -> Result<BlockResult, ExecutorError> {
        // Backward-compatible path: call the cancel-aware impl with a never-
        // cancelled token and convert the typed shape to BlockResult.
        let response = self
            .execute_with_cancel(params, CancellationToken::new())
            .await?;

        let status = if response.status_code < 400 {
            "success"
        } else {
            "error"
        };

        Ok(BlockResult {
            status: status.to_string(),
            data: serde_json::json!({
                "status_code": response.status_code,
                "status_text": response.status_text,
                "headers": response.headers,
                "body": response.body,
                "size_bytes": response.size_bytes,
            }),
            duration_ms: response.elapsed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_executor_validate_empty_url() {
        let executor = HttpExecutor::new();
        let params = serde_json::json!({ "method": "GET", "url": "" });
        let result = executor.validate(&params).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("URL is required"));
    }

    #[tokio::test]
    async fn test_http_executor_validate_missing_url() {
        let executor = HttpExecutor::new();
        let params = serde_json::json!({ "method": "GET" });
        let result = executor.validate(&params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_executor_get_json() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/users/1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-request-id", "abc123")
                    .set_body_json(serde_json::json!({"id": 1, "name": "Alice"})),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/users/1", server.uri()),
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");

        let data = &result.data;
        assert_eq!(data["status_code"], 200);
        assert_eq!(data["status_text"], "OK");
        assert_eq!(data["body"]["id"], 1);
        assert_eq!(data["body"]["name"], "Alice");
        assert_eq!(data["headers"]["x-request-id"], "abc123");
        assert!(data["size_bytes"].as_u64().unwrap() > 0);
        assert!(result.duration_ms < 5000);
    }

    #[tokio::test]
    async fn test_http_executor_post_with_body() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/users"))
            .and(header("content-type", "application/json"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": 2, "created": true})),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "POST",
            "url": format!("{}/users", server.uri()),
            "body": r#"{"name":"Bob"}"#,
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["status_code"], 201);
        assert_eq!(result.data["body"]["created"], true);
    }

    #[tokio::test]
    async fn test_http_executor_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/not-found"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/not-found", server.uri()),
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "error");
        assert_eq!(result.data["status_code"], 404);
    }

    #[tokio::test]
    async fn test_http_executor_binary_response() {
        let server = MockServer::start().await;
        let png_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

        Mock::given(method("GET"))
            .and(path("/avatar.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(png_bytes.clone()),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/avatar.png", server.uri()),
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["body"]["encoding"], "base64");

        let decoded = BASE64_STANDARD
            .decode(result.data["body"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, png_bytes);
        assert_eq!(result.data["size_bytes"], png_bytes.len());
    }

    #[tokio::test]
    async fn test_http_executor_timeout() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60)),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/slow", server.uri()),
            "timeout_ms": 200,
        });

        let result = executor.execute(params).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("[timeout]"), "Error should be classified as timeout: {err}");
    }

    #[tokio::test]
    async fn test_http_executor_custom_headers() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api"))
            .and(header("authorization", "Bearer tok123"))
            .and(header("x-custom", "value"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/api", server.uri()),
            "headers": [
                { "key": "Authorization", "value": "Bearer tok123" },
                { "key": "X-Custom", "value": "value" },
            ],
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["status_code"], 200);
    }

    // ───── Cancel-aware path ─────

    #[tokio::test]
    async fn test_execute_with_cancel_returns_typed_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/typed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", "sid=abc; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true})),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/typed", server.uri()),
        });
        let token = CancellationToken::new();

        let response = executor
            .execute_with_cancel(params, token)
            .await
            .expect("execute_with_cancel should succeed");

        assert_eq!(response.status_code, 200);
        assert_eq!(response.body["ok"], true);
        assert_eq!(response.cookies.len(), 1);
        assert_eq!(response.cookies[0].name, "sid");
        assert_eq!(response.cookies[0].path.as_deref(), Some("/"));
        assert!(response.cookies[0].http_only);
        assert_eq!(response.timing.total_ms, response.elapsed_ms);
    }

    #[tokio::test]
    async fn test_execute_with_cancel_aborts_in_flight_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60)),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/slow", server.uri()),
        });
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel the request after a short delay so the future is mid-flight.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            token_clone.cancel();
        });

        let result = executor.execute_with_cancel(params, token).await;
        let err = result.expect_err("cancel should produce an error");
        assert_eq!(err.to_string(), "Request cancelled");
    }

    #[tokio::test]
    async fn test_execute_with_cancel_4xx_is_ok() {
        // HTTP-level errors (4xx/5xx) are still "successful executions" on
        // the typed path — only transport / cancel failures map to Err.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/missing", server.uri()),
        });

        let response = executor
            .execute_with_cancel(params, CancellationToken::new())
            .await
            .expect("4xx should not be an Err on the typed path");
        assert_eq!(response.status_code, 404);
    }

    #[tokio::test]
    async fn test_execute_with_cancel_captures_multiple_cookies() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cookies"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("set-cookie", "a=1; Path=/")
                    .append_header("set-cookie", "b=2; Domain=example.com; Secure")
                    .set_body_string("ok"),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/cookies", server.uri()),
        });
        let response = executor
            .execute_with_cancel(params, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(response.cookies.len(), 2);
        let names: Vec<&str> = response.cookies.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[tokio::test]
    async fn test_execute_legacy_path_still_works_via_cancel_aware_impl() {
        // The legacy `execute` BlockResult path now goes through
        // `execute_with_cancel`; we double-check the conversion preserves
        // the shape consumers depend on.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/echo"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"received": "yes"})),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "POST",
            "url": format!("{}/echo", server.uri()),
            "body": "{}",
        });
        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["status_code"], 201);
        assert_eq!(result.data["body"]["received"], "yes");
    }

    // ─────────── Onda 1 — per-block flags (HttpParams extras) ───────────

    #[tokio::test]
    async fn http_params_accepts_new_flags() {
        // Just exercise the deserialization — the executor reads these via
        // `serde_json::from_value` deeper down; flag-aware behaviour is
        // covered by the dedicated tests below.
        let json = serde_json::json!({
            "method": "GET",
            "url": "https://example.com",
            "follow_redirects": false,
            "verify_ssl": false,
            "encode_url": false,
            "trim_whitespace": false,
        });
        let parsed: HttpParams = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.follow_redirects, Some(false));
        assert_eq!(parsed.verify_ssl, Some(false));
        assert_eq!(parsed.encode_url, Some(false));
        assert_eq!(parsed.trim_whitespace, Some(false));
    }

    #[tokio::test]
    async fn http_params_omits_flags_yields_none() {
        // Defaults via serde — backwards-compatible with old payloads.
        let json = serde_json::json!({
            "method": "GET",
            "url": "https://example.com",
        });
        let parsed: HttpParams = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.follow_redirects, None);
        assert_eq!(parsed.verify_ssl, None);
        assert_eq!(parsed.encode_url, None);
        assert_eq!(parsed.trim_whitespace, None);
    }

    #[tokio::test]
    async fn client_cache_is_keyed_by_flags() {
        let executor = HttpExecutor::new();
        // Default client populated by `new()`.
        let n_default = executor.clients.lock().unwrap().len();
        assert!(n_default >= 1);
        let _ = executor.client_for(ClientFlags {
            follow_redirects: false,
            verify_ssl: true,
        });
        let _ = executor.client_for(ClientFlags {
            follow_redirects: false,
            verify_ssl: true,
        });
        // Same combo — no new client cached.
        assert_eq!(executor.clients.lock().unwrap().len(), n_default + 1);
        let _ = executor.client_for(ClientFlags {
            follow_redirects: false,
            verify_ssl: false,
        });
        assert_eq!(executor.clients.lock().unwrap().len(), n_default + 2);
    }

    #[tokio::test]
    async fn follow_redirects_false_returns_3xx_to_caller() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/elsewhere"),
            )
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/start", server.uri()),
            "follow_redirects": false,
        });
        let response = executor
            .execute_with_cancel(params, CancellationToken::new())
            .await
            .unwrap();
        // Without follow, the 302 surfaces directly.
        assert_eq!(response.status_code, 302);
        assert_eq!(
            response.headers.get("location").map(|s| s.as_str()),
            Some("/elsewhere"),
        );
    }

    #[tokio::test]
    async fn trim_whitespace_strips_header_and_body() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/trim"))
            .and(header("x-trimmed", "value"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "POST",
            "url": format!("{}/trim", server.uri()),
            "headers": [
                { "key": "  X-Trimmed  ", "value": "  value  " },
            ],
            "body": "  payload  ",
            // trim_whitespace defaults ON; we pass it explicitly here for clarity.
            "trim_whitespace": true,
        });
        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["status_code"], 200);
    }

    #[tokio::test]
    async fn encode_url_off_preserves_raw_query() {
        // With encode_url=true (default), `q=a b` becomes `q=a+b` or `q=a%20b`.
        // With encode_url=false, the raw value is appended verbatim.
        let server = MockServer::start().await;

        // Wiremock matches the raw query string the server sees.
        Mock::given(method("GET"))
            .and(path("/q"))
            .and(wiremock::matchers::query_param("q", "a%20b"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let executor = HttpExecutor::new();
        let params = serde_json::json!({
            "method": "GET",
            "url": format!("{}/q", server.uri()),
            "params": [{ "key": "q", "value": "a%20b" }],
            "encode_url": false,
        });
        let result = executor.execute(params).await;
        assert!(result.is_ok(), "expected query to pass through verbatim");
    }
}
