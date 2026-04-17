use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{BlockResult, Executor, ExecutorError};

#[derive(Debug, Deserialize)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct Extraction {
    name: String,
    path: String,
}

#[derive(Debug, Deserialize, Default)]
struct E2eExpect {
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    json: Vec<KeyValue>,
    #[serde(default)]
    body_contains: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct E2eStep {
    name: String,
    method: String,
    url: String,
    #[serde(default)]
    params: Vec<KeyValue>,
    #[serde(default)]
    headers: Vec<KeyValue>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    expect: E2eExpect,
    #[serde(default)]
    extract: Vec<Extraction>,
}

#[derive(Debug, Deserialize)]
struct E2eParams {
    base_url: String,
    #[serde(default)]
    headers: Vec<KeyValue>,
    steps: Vec<E2eStep>,
}

#[derive(Debug, Serialize)]
struct StepResult {
    name: String,
    passed: bool,
    errors: Vec<String>,
    status_code: u16,
    elapsed_ms: u64,
    response_body: serde_json::Value,
    extractions: HashMap<String, serde_json::Value>,
}

/// Navigate a JSON value by a dot-separated path (e.g. "response.data.id").
fn navigate_json(value: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value.clone();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        // Try array index
        if let Ok(idx) = part.parse::<usize>() {
            current = current.get(idx)?.clone();
        } else {
            current = current.get(part)?.clone();
        }
    }
    Some(current)
}

/// Resolve `{{var}}` placeholders against extracted variables from prior steps.
fn resolve_variables(text: &str, variables: &HashMap<String, serde_json::Value>) -> String {
    let mut result = text.to_string();
    for (name, value) in variables {
        let placeholder = format!("{{{{{}}}}}", name);
        let replacement = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

pub struct E2eExecutor {
    client: Client,
}

impl Default for E2eExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl E2eExecutor {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn execute_step(
        &self,
        step: &E2eStep,
        base_url: &str,
        default_headers: &[KeyValue],
        variables: &HashMap<String, serde_json::Value>,
    ) -> StepResult {
        let mut errors = Vec::new();
        let mut extractions = HashMap::new();

        // Build URL: base_url + step.url
        let raw_url = format!(
            "{}{}",
            base_url.trim_end_matches('/'),
            resolve_variables(&step.url, variables)
        );
        let resolved_url = resolve_variables(&raw_url, variables);

        let mut url = match reqwest::Url::parse(&resolved_url) {
            Ok(u) => u,
            Err(e) => {
                return StepResult {
                    name: step.name.clone(),
                    passed: false,
                    errors: vec![format!("Invalid URL: {e}")],
                    status_code: 0,
                    elapsed_ms: 0,
                    response_body: serde_json::Value::Null,
                    extractions,
                };
            }
        };

        // Append query params
        for kv in &step.params {
            if !kv.key.is_empty() {
                let value = resolve_variables(&kv.value, variables);
                url.query_pairs_mut().append_pair(&kv.key, &value);
            }
        }

        // Build method
        let method = match step.method.parse::<reqwest::Method>() {
            Ok(m) => m,
            Err(e) => {
                return StepResult {
                    name: step.name.clone(),
                    passed: false,
                    errors: vec![format!("Invalid method: {e}")],
                    status_code: 0,
                    elapsed_ms: 0,
                    response_body: serde_json::Value::Null,
                    extractions,
                };
            }
        };

        let mut req = self.client.request(method.clone(), url);

        // Merge headers: defaults first, then step overrides
        let step_header_keys: Vec<String> = step
            .headers
            .iter()
            .map(|h| h.key.to_lowercase())
            .collect();

        for kv in default_headers {
            if !kv.key.is_empty() && !step_header_keys.contains(&kv.key.to_lowercase()) {
                let value = resolve_variables(&kv.value, variables);
                req = req.header(&kv.key, &value);
            }
        }
        for kv in &step.headers {
            if !kv.key.is_empty() {
                let value = resolve_variables(&kv.value, variables);
                req = req.header(&kv.key, &value);
            }
        }

        // Body for methods that support it
        let has_body = matches!(
            method,
            reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
        );
        let resolved_body = resolve_variables(&step.body, variables);
        if has_body && !resolved_body.is_empty() {
            req = req
                .header("Content-Type", "application/json")
                .body(resolved_body);
        }

        // Execute
        let start = Instant::now();
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return StepResult {
                    name: step.name.clone(),
                    passed: false,
                    errors: vec![format!("Request failed: {e}")],
                    status_code: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    response_body: serde_json::Value::Null,
                    extractions,
                };
            }
        };
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let status_code = response.status().as_u16();

        // Read body
        let body_text = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return StepResult {
                    name: step.name.clone(),
                    passed: false,
                    errors: vec![format!("Failed to read response body: {e}")],
                    status_code,
                    elapsed_ms,
                    response_body: serde_json::Value::Null,
                    extractions,
                };
            }
        };

        let response_body: serde_json::Value = serde_json::from_str(&body_text)
            .unwrap_or_else(|_| serde_json::Value::String(body_text));

        // Validate expectations
        // 1. Status
        if let Some(expected_status) = step.expect.status {
            if status_code != expected_status {
                errors.push(format!(
                    "Status: expected {expected_status}, got {status_code}"
                ));
            }
        }

        // 2. JSON match
        for kv in &step.expect.json {
            if kv.key.is_empty() {
                continue;
            }
            let resolved_expected = resolve_variables(&kv.value, variables);
            match navigate_json(&response_body, &kv.key) {
                Some(actual) => {
                    let actual_str = match &actual {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if actual_str != resolved_expected {
                        errors.push(format!(
                            "JSON {}: expected \"{}\", got \"{}\"",
                            kv.key, resolved_expected, actual_str
                        ));
                    }
                }
                None => {
                    errors.push(format!("JSON {}: path not found in response", kv.key));
                }
            }
        }

        // 3. Body contains
        let body_str = match &response_body {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        for needle in &step.expect.body_contains {
            if !needle.is_empty() && !body_str.contains(needle) {
                errors.push(format!("Body does not contain: \"{}\"", needle));
            }
        }

        // Process extractions
        for ext in &step.extract {
            if ext.name.is_empty() || ext.path.is_empty() {
                continue;
            }
            match navigate_json(&response_body, &ext.path) {
                Some(value) => {
                    extractions.insert(ext.name.clone(), value);
                }
                None => {
                    errors.push(format!(
                        "Extract \"{}\": path \"{}\" not found",
                        ext.name, ext.path
                    ));
                }
            }
        }

        StepResult {
            name: step.name.clone(),
            passed: errors.is_empty(),
            errors,
            status_code,
            elapsed_ms,
            response_body,
            extractions,
        }
    }
}

#[async_trait]
impl Executor for E2eExecutor {
    fn block_type(&self) -> &str {
        "e2e"
    }

    async fn validate(&self, params: &serde_json::Value) -> Result<(), String> {
        let p: E2eParams =
            serde_json::from_value(params.clone()).map_err(|e| format!("Invalid params: {e}"))?;
        if p.base_url.trim().is_empty() {
            return Err("Base URL is required".to_string());
        }
        if p.steps.is_empty() {
            return Err("At least one step is required".to_string());
        }
        Ok(())
    }

    async fn execute(
        &self,
        params: serde_json::Value,
    ) -> Result<BlockResult, ExecutorError> {
        let p: E2eParams = serde_json::from_value(params)
            .map_err(|e| ExecutorError(format!("Invalid params: {e}")))?;

        let start = Instant::now();
        let mut step_results: Vec<StepResult> = Vec::new();
        let mut variables: HashMap<String, serde_json::Value> = HashMap::new();

        for step in &p.steps {
            let result =
                self.execute_step(step, &p.base_url, &p.headers, &variables).await;

            // Accumulate extractions for subsequent steps
            for (name, value) in &result.extractions {
                variables.insert(name.clone(), value.clone());
            }

            step_results.push(result);
        }

        let total = step_results.len();
        let passed = step_results.iter().filter(|r| r.passed).count();
        let all_passed = passed == total;

        let status = if all_passed { "success" } else { "error" };

        Ok(BlockResult {
            status: status.to_string(),
            data: serde_json::json!({
                "passed": passed,
                "total": total,
                "steps": step_results,
            }),
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_validate_empty_base_url() {
        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": "",
            "steps": [{ "name": "s1", "method": "GET", "url": "/test" }]
        });
        let result = executor.validate(&params).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Base URL is required"));
    }

    #[tokio::test]
    async fn test_validate_no_steps() {
        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": "http://localhost",
            "steps": []
        });
        let result = executor.validate(&params).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("At least one step"));
    }

    #[tokio::test]
    async fn test_all_steps_pass() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/users"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"id": 1, "name": "Alice"}])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/users/1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": 1, "name": "Alice"})),
            )
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "steps": [
                {
                    "name": "List users",
                    "method": "GET",
                    "url": "/users",
                    "expect": { "status": 200 }
                },
                {
                    "name": "Get user",
                    "method": "GET",
                    "url": "/users/1",
                    "expect": {
                        "status": 200,
                        "json": [{ "key": "name", "value": "Alice" }]
                    }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["passed"], 2);
        assert_eq!(result.data["total"], 2);
    }

    #[tokio::test]
    async fn test_step_failure_continues() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fail"))
            .respond_with(ResponseTemplate::new(500).set_body_string("error"))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "steps": [
                {
                    "name": "Failing step",
                    "method": "GET",
                    "url": "/fail",
                    "expect": { "status": 200 }
                },
                {
                    "name": "Passing step",
                    "method": "GET",
                    "url": "/ok",
                    "expect": { "status": 200 }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "error");
        assert_eq!(result.data["passed"], 1);
        assert_eq!(result.data["total"], 2);

        let steps = result.data["steps"].as_array().unwrap();
        assert!(!steps[0]["passed"].as_bool().unwrap());
        assert!(steps[1]["passed"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_extraction_between_steps() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/orders"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": "order-42", "status": "pending"})),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/orders/order-42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "order-42", "status": "pending"})),
            )
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "steps": [
                {
                    "name": "Create order",
                    "method": "POST",
                    "url": "/orders",
                    "body": "{\"product\": \"abc\"}",
                    "expect": { "status": 201 },
                    "extract": [{ "name": "order_id", "path": "id" }]
                },
                {
                    "name": "Get order",
                    "method": "GET",
                    "url": "/orders/{{order_id}}",
                    "expect": {
                        "status": 200,
                        "json": [{ "key": "status", "value": "pending" }]
                    }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["passed"], 2);

        let steps = result.data["steps"].as_array().unwrap();
        // First step should have extracted order_id
        let ext = &steps[0]["extractions"];
        assert_eq!(ext["order_id"], "order-42");
    }

    #[tokio::test]
    async fn test_body_contains_assertion() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>Hello World</body></html>"),
            )
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "steps": [
                {
                    "name": "Check HTML",
                    "method": "GET",
                    "url": "/html",
                    "expect": {
                        "status": 200,
                        "body_contains": ["Hello World", "missing text"]
                    }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "error");

        let steps = result.data["steps"].as_array().unwrap();
        assert!(!steps[0]["passed"].as_bool().unwrap());
        let errors = steps[0]["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].as_str().unwrap().contains("missing text"));
    }

    #[tokio::test]
    async fn test_default_headers_with_override() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api"))
            .and(header("authorization", "Bearer custom-token"))
            .and(header("x-default", "yes"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "headers": [
                { "key": "Authorization", "value": "Bearer default-token" },
                { "key": "X-Default", "value": "yes" }
            ],
            "steps": [
                {
                    "name": "Override auth",
                    "method": "GET",
                    "url": "/api",
                    "headers": [
                        { "key": "Authorization", "value": "Bearer custom-token" }
                    ],
                    "expect": { "status": 200 }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        assert_eq!(result.status, "success");
    }

    #[tokio::test]
    async fn test_json_path_not_found() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/data"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"a": 1})),
            )
            .mount(&server)
            .await;

        let executor = E2eExecutor::new();
        let params = serde_json::json!({
            "base_url": server.uri(),
            "steps": [
                {
                    "name": "Check missing path",
                    "method": "GET",
                    "url": "/data",
                    "expect": {
                        "json": [{ "key": "nonexistent.path", "value": "something" }]
                    }
                }
            ]
        });

        let result = executor.execute(params).await.unwrap();
        let steps = result.data["steps"].as_array().unwrap();
        assert!(!steps[0]["passed"].as_bool().unwrap());
        let errors = steps[0]["errors"].as_array().unwrap();
        assert!(errors[0].as_str().unwrap().contains("path not found"));
    }
}
