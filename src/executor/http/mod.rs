pub mod types;

use async_trait::async_trait;
use base64::prelude::*;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
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

pub struct HttpExecutor {
    client: Client,
}

impl Default for HttpExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpExecutor {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
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
        let req = build_request(&self.client, &p)?;

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
    let mut url = reqwest::Url::parse(&p.url)
        .map_err(|e| ExecutorError(format!("Invalid URL: {e}")))?;
    for kv in &p.params {
        if !kv.key.is_empty() {
            url.query_pairs_mut().append_pair(&kv.key, &kv.value);
        }
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
        if !kv.key.is_empty() {
            req = req.header(&kv.key, &kv.value);
        }
    }

    let has_body = matches!(
        method,
        reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
    );
    if has_body && !p.body.is_empty() {
        // Only inject default Content-Type when the user didn't set one.
        let user_set_ct = p
            .headers
            .iter()
            .any(|kv| kv.key.eq_ignore_ascii_case("content-type"));
        if !user_set_ct {
            req = req.header("Content-Type", "application/json");
        }
        req = req.body(p.body.clone());
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
}
