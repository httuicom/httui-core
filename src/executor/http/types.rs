//! Response shape for the `http` block type.
//!
//! Stage 2 of the HTTP block redesign (see `docs/http-block-redesign.md`).
//! This shape is prepared for the streamed/cancel-aware execution path —
//! today the executor still buffers the body in memory and emits a single
//! `HttpChunk::Complete` on the channel, mirroring the DB block's stage 3
//! plumbing. Body chunking and a richer timing breakdown land in a later
//! stage; the JSON shape is forward-compatible.
//!
//! Stability:
//! - `HttpResponse` field set is final for V1.
//! - `cookies` is captured from `Set-Cookie` response headers; the persistent
//!   cookie jar (V2 in the spec) is not implemented here.
//! - `timing` ships with `total_ms` only for now. Sub-fields (`dns_ms`,
//!   `connect_ms`, `tls_ms`, `ttfb_ms`) are reserved and may appear in
//!   later runs without breaking existing consumers (all are `Option`).

use serde::{Deserialize, Serialize};

/// Per-execution timing breakdown.
///
/// V1 ships `total_ms` only — sub-fields stay `None` until a reqwest
/// middleware exposes connect/TLS/TTFB hooks. Consumers should treat
/// missing fields as "unknown", not "zero".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TimingBreakdown {
    pub total_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
}

/// A cookie captured from a `Set-Cookie` response header.
///
/// V1 stores raw attribute strings (`domain`, `path`, `expires`) without
/// RFC-6265 parsing. The persistent cookie jar (V2) will normalize these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
}

/// Top-level response shape for an `http` block execution.
///
/// `body` carries either the parsed JSON, the response text, or a
/// base64-encoded binary payload (`{ encoding: "base64", data: "..." }`),
/// matching the legacy shape so existing references (`{{req.response.body.foo}}`)
/// keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status_code: u16,
    pub status_text: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: serde_json::Value,
    pub size_bytes: u64,
    pub elapsed_ms: u64,
    pub timing: TimingBreakdown,
    #[serde(default)]
    pub cookies: Vec<Cookie>,
}

/// Streaming chunk emitted to a `tauri::Channel<HttpChunk>` during execution.
///
/// V1 ships the minimal cancel-aware variants (mirror of `DbChunk`). Richer
/// streaming variants (`Headers`, `BodyChunk`) are reserved for a follow-up
/// stage that consumes reqwest's body stream incrementally.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HttpChunk {
    /// Terminal chunk containing the full response. Consumer should close
    /// the subscription after receiving this.
    Complete(HttpResponse),
    /// Terminal chunk indicating the execution failed before completing.
    Error { message: String },
    /// Terminal chunk indicating the execution was cancelled.
    Cancelled,
}

/// Parse a single `Set-Cookie` header value into a `Cookie`.
///
/// RFC 6265-aware enough for V1 — splits on `;`, lowercases attribute names,
/// preserves case for the cookie name/value. Returns `None` if the header
/// has no `name=value` pair.
pub fn parse_set_cookie(header: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let nv = parts.next()?.trim();
    let eq = nv.find('=')?;
    let name = nv[..eq].trim().to_string();
    let value = nv[eq + 1..].trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut cookie = Cookie {
        name,
        value,
        domain: None,
        path: None,
        expires: None,
        secure: false,
        http_only: false,
    };

    for attr in parts {
        let attr = attr.trim();
        let (key, val) = match attr.find('=') {
            Some(i) => (&attr[..i], Some(attr[i + 1..].trim().to_string())),
            None => (attr, None),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" => cookie.domain = val,
            "path" => cookie.path = val,
            "expires" => cookie.expires = val,
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            _ => {}
        }
    }

    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_chunk_complete_serializes_inline() {
        let response = HttpResponse {
            status_code: 200,
            status_text: "OK".to_string(),
            headers: Default::default(),
            body: serde_json::json!({"hello": "world"}),
            size_bytes: 17,
            elapsed_ms: 42,
            timing: TimingBreakdown {
                total_ms: 42,
                ..Default::default()
            },
            cookies: vec![],
        };
        let chunk = HttpChunk::Complete(response);
        let v = serde_json::to_value(&chunk).unwrap();
        assert_eq!(v["kind"], "complete");
        assert_eq!(v["status_code"], 200);
        assert_eq!(v["body"]["hello"], "world");
        assert_eq!(v["timing"]["total_ms"], 42);
    }

    #[test]
    fn http_chunk_error_and_cancelled_serialize() {
        let err = HttpChunk::Error { message: "boom".to_string() };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "error");
        assert_eq!(v["message"], "boom");

        let cancelled = HttpChunk::Cancelled;
        let v = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(v["kind"], "cancelled");
    }

    #[test]
    fn parse_set_cookie_basic() {
        let c = parse_set_cookie("session=abc123").unwrap();
        assert_eq!(c.name, "session");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.domain, None);
        assert!(!c.secure);
    }

    #[test]
    fn parse_set_cookie_with_attrs() {
        let c =
            parse_set_cookie("sid=xyz; Domain=example.com; Path=/; Secure; HttpOnly")
                .unwrap();
        assert_eq!(c.name, "sid");
        assert_eq!(c.value, "xyz");
        assert_eq!(c.domain.as_deref(), Some("example.com"));
        assert_eq!(c.path.as_deref(), Some("/"));
        assert!(c.secure);
        assert!(c.http_only);
    }

    #[test]
    fn parse_set_cookie_with_expires() {
        let c = parse_set_cookie(
            "foo=bar; Expires=Wed, 21 Oct 2026 07:28:00 GMT",
        )
        .unwrap();
        assert_eq!(c.expires.as_deref(), Some("Wed, 21 Oct 2026 07:28:00 GMT"));
    }

    #[test]
    fn parse_set_cookie_rejects_no_equals() {
        assert!(parse_set_cookie("no-equals").is_none());
    }

    #[test]
    fn parse_set_cookie_rejects_empty_name() {
        assert!(parse_set_cookie("=value").is_none());
    }

    #[test]
    fn timing_breakdown_default_omits_optional_fields() {
        let t = TimingBreakdown::default();
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["total_ms"], 0);
        assert!(v.get("dns_ms").is_none());
        assert!(v.get("connect_ms").is_none());
    }
}
