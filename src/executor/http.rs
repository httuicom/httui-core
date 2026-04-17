use async_trait::async_trait;
use base64::prelude::*;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

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
        let p: HttpParams = serde_json::from_value(params)
            .map_err(|e| ExecutorError(format!("Invalid params: {e}")))?;

        // Build URL with query params
        let mut url =
            reqwest::Url::parse(&p.url).map_err(|e| ExecutorError(format!("Invalid URL: {e}")))?;
        for kv in &p.params {
            if !kv.key.is_empty() {
                url.query_pairs_mut().append_pair(&kv.key, &kv.value);
            }
        }

        // Build request
        let method = p
            .method
            .parse::<reqwest::Method>()
            .map_err(|e| ExecutorError(format!("Invalid method: {e}")))?;

        let mut req = self.client.request(method.clone(), url);

        // Per-request timeout override
        if let Some(ms) = p.timeout_ms {
            req = req.timeout(Duration::from_millis(ms));
        }

        // Add headers
        for kv in &p.headers {
            if !kv.key.is_empty() {
                req = req.header(&kv.key, &kv.value);
            }
        }

        // Add body for methods that support it
        let has_body = matches!(
            method,
            reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
        );
        if has_body && !p.body.is_empty() {
            req = req
                .header("Content-Type", "application/json")
                .body(p.body);
        }

        // Execute
        let start = Instant::now();
        let response = req.send().await.map_err(|e| {
            ExecutorError(format!("[{}] {}", classify_reqwest_error(&e), e))
        })?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let status_code = response.status().as_u16();
        let status_text = response
            .status()
            .canonical_reason()
            .unwrap_or("")
            .to_string();

        // Collect response headers
        let resp_headers: HashMap<String, String> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let content_type = resp_headers
            .get("content-type")
            .map(|s| s.as_str())
            .unwrap_or("");

        // Read body as bytes for size tracking and binary detection
        let bytes = response.bytes().await.map_err(|e| {
            ExecutorError(format!("[{}] {}", classify_reqwest_error(&e), e))
        })?;
        let size_bytes = bytes.len();

        let body_value = if is_binary_content_type(content_type) {
            serde_json::json!({
                "encoding": "base64",
                "data": BASE64_STANDARD.encode(&bytes),
            })
        } else {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|_| serde_json::Value::String(text))
        };

        let status = if status_code < 400 {
            "success"
        } else {
            "error"
        };

        Ok(BlockResult {
            status: status.to_string(),
            data: serde_json::json!({
                "status_code": status_code,
                "status_text": status_text,
                "headers": resp_headers,
                "body": body_value,
                "size_bytes": size_bytes,
            }),
            duration_ms,
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
}
