use crate::blocks::parser::ParsedBlock;

/// Serialize a [`ParsedBlock`] back to its fenced markdown form.
///
/// Output is canonical and deterministic: parsing the result and
/// re-serializing yields byte-identical bytes (idempotent), and parsing
/// preserves the semantic shape of the original block.
///
/// Format per block type:
/// - `db` family — info string `<type> [alias=…] [connection=…] [limit=…] [timeout=…] [display=…]`,
///   body is the raw SQL stored in `params.query`. Mirrors the canonical
///   form documented in `src/lib/blocks/db-fence.ts`.
/// - `http` — info string `<type> [alias=…] [displayMode=…]`,
///   body is `params` rendered as compact JSON.
/// - Unknown types — same as http (JSON body fallback). New block
///   types can ship without a dedicated serializer until their fence
///   shape is finalized.
pub fn serialize_block(block: &ParsedBlock) -> String {
    if is_db_block(&block.block_type) {
        serialize_db_block(block)
    } else {
        serialize_json_block(block)
    }
}

fn is_db_block(block_type: &str) -> bool {
    block_type == "db" || block_type.starts_with("db-")
}

fn serialize_db_block(block: &ParsedBlock) -> String {
    let mut info = block.block_type.clone();

    if let Some(alias) = block.alias.as_deref().filter(|s| !s.is_empty()) {
        info.push_str(" alias=");
        info.push_str(alias);
    }

    let params = block.params.as_object();
    if let Some(obj) = params {
        if let Some(conn) = obj
            .get("connection_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            info.push_str(" connection=");
            info.push_str(conn);
        }
        if let Some(limit) = obj.get("limit").and_then(|v| v.as_u64()) {
            info.push_str(" limit=");
            info.push_str(&limit.to_string());
        }
        if let Some(timeout) = obj.get("timeout_ms").and_then(|v| v.as_u64()) {
            info.push_str(" timeout=");
            info.push_str(&timeout.to_string());
        }
    }

    if let Some(display) = block.display_mode.as_deref().filter(|s| !s.is_empty()) {
        info.push_str(" display=");
        info.push_str(display);
    }

    let body = params
        .and_then(|o| o.get("query"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    format!("```{info}\n{body}\n```")
}

fn serialize_json_block(block: &ParsedBlock) -> String {
    let mut info = block.block_type.clone();

    if let Some(alias) = block.alias.as_deref().filter(|s| !s.is_empty()) {
        info.push_str(" alias=");
        info.push_str(alias);
    }
    if let Some(display) = block.display_mode.as_deref().filter(|s| !s.is_empty()) {
        info.push_str(" displayMode=");
        info.push_str(display);
    }

    let body = serde_json::to_string(&block.params).unwrap_or_else(|_| "null".to_string());

    format!("```{info}\n{body}\n```")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::parser::parse_blocks;
    use serde_json::json;

    fn assert_semantic_roundtrip(md: &str) {
        let parsed = parse_blocks(md);
        assert_eq!(parsed.len(), 1, "expected exactly 1 block in fixture");
        let serialized = serialize_block(&parsed[0]);
        let reparsed = parse_blocks(&serialized);
        assert_eq!(reparsed.len(), 1, "roundtrip must yield 1 block");
        assert_eq!(reparsed[0].block_type, parsed[0].block_type);
        assert_eq!(reparsed[0].alias, parsed[0].alias);
        assert_eq!(reparsed[0].display_mode, parsed[0].display_mode);
        assert_eq!(reparsed[0].params, parsed[0].params);
    }

    fn assert_idempotent(md: &str) {
        let parsed = parse_blocks(md);
        let s1 = serialize_block(&parsed[0]);
        let reparsed = parse_blocks(&s1);
        let s2 = serialize_block(&reparsed[0]);
        assert_eq!(s1, s2, "serialization must be idempotent");
    }

    // ─── Roundtrip across all 3 block types ───

    #[test]
    fn roundtrip_http_simple() {
        let md = "```http alias=login\n{\"method\":\"POST\",\"url\":\"https://api.test.com/login\",\"params\":[],\"headers\":[],\"body\":\"\"}\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    #[test]
    fn roundtrip_http_with_display_mode() {
        let md = "```http alias=login displayMode=split\n{\"method\":\"GET\",\"url\":\"https://x.com\",\"params\":[],\"headers\":[],\"body\":\"\"}\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    #[test]
    fn roundtrip_http_without_alias() {
        let md = "```http\n{\"method\":\"GET\",\"url\":\"https://x.com\",\"params\":[],\"headers\":[],\"body\":\"\"}\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    #[test]
    fn roundtrip_db_postgres_full() {
        let md = "```db-postgres alias=db1 connection=prod limit=100 timeout=30000 display=split\nSELECT *\nFROM users\nWHERE id > 10\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    #[test]
    fn roundtrip_db_minimal() {
        let md = "```db-mysql\nSELECT 1\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    #[test]
    fn roundtrip_db_with_display_only() {
        let md = "```db alias=q display=output\nSELECT 1\n```\n";
        assert_semantic_roundtrip(md);
        assert_idempotent(md);
    }

    // ─── DB info-string canonical order ───

    #[test]
    fn db_info_string_emits_canonical_order() {
        let parsed = parse_blocks(
            "```db-postgres alias=a display=split timeout=5000 limit=50 connection=prod\nSELECT 1\n```\n",
        );
        let out = serialize_block(&parsed[0]);
        assert!(
            out.starts_with(
                "```db-postgres alias=a connection=prod limit=50 timeout=5000 display=split\n"
            ),
            "got: {out}"
        );
    }

    #[test]
    fn db_info_string_omits_missing_fields() {
        let parsed = parse_blocks("```db-postgres alias=a\nSELECT 1\n```\n");
        let out = serialize_block(&parsed[0]);
        assert_eq!(out, "```db-postgres alias=a\nSELECT 1\n```");
    }

    #[test]
    fn db_legacy_body_normalizes_to_canonical_form() {
        // Legacy JSON body: `params` already has connection_id/query/timeout_ms.
        // Serializer must emit the new raw-SQL canonical form regardless.
        let parsed = parse_blocks(
            "```db-postgres alias=u\n{\"connection_id\":\"x\",\"query\":\"SELECT 1\",\"timeout_ms\":5000}\n```\n",
        );
        let out = serialize_block(&parsed[0]);
        assert_eq!(
            out,
            "```db-postgres alias=u connection=x timeout=5000\nSELECT 1\n```"
        );
    }

    // ─── Forward-compat for unknown types ───

    #[test]
    fn unknown_block_type_serializes_as_json() {
        // Future block types (e.g. graphql) without a dedicated serializer
        // fall back to JSON-body. This guarantees adding a new type to the
        // parser doesn't break round-trip.
        let block = ParsedBlock {
            block_type: "graphql".to_string(),
            alias: Some("q1".to_string()),
            display_mode: None,
            params: json!({"query": "{ user { id } }"}),
            line_start: 0,
            line_end: 0,
        };
        let out = serialize_block(&block);
        assert_eq!(
            out,
            "```graphql alias=q1\n{\"query\":\"{ user { id } }\"}\n```"
        );
    }
}
