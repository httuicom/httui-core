use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A parsed executable block extracted from a markdown file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedBlock {
    pub block_type: String,
    pub alias: Option<String>,
    pub display_mode: Option<String>,
    pub params: serde_json::Value,
    pub line_start: usize,
    pub line_end: usize,
}

/// Known executable block types.
const EXECUTABLE_TYPES: &[&str] = &[
    "http", "db", "db-postgres", "db-mysql", "db-sqlite", "e2e",
];

/// Parse all executable blocks from a markdown string.
///
/// Scans for fenced code blocks with known executable types in the info string.
/// Format: ```<block_type> [key=value ...]
///         <JSON body>
///         ```
pub fn parse_blocks(markdown: &str) -> Vec<ParsedBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Detect opening fence (``` with info string)
        if let Some(info) = strip_fence_open(trimmed) {
            let (block_type, attrs) = parse_info_string(info);

            if EXECUTABLE_TYPES.contains(&block_type.as_str()) {
                let line_start = i;

                // Collect body lines until closing fence
                let mut body_lines = Vec::new();
                i += 1;
                while i < lines.len() {
                    let l = lines[i].trim();
                    if l == "```" || l.starts_with("```") && l.chars().skip(3).all(|c| c == '`') {
                        break;
                    }
                    body_lines.push(lines[i]);
                    i += 1;
                }
                let line_end = i;

                // Parse JSON body
                let body = body_lines.join("\n");
                let params = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

                blocks.push(ParsedBlock {
                    block_type,
                    alias: attrs.get("alias").cloned(),
                    display_mode: attrs.get("displayMode").cloned(),
                    params,
                    line_start,
                    line_end,
                });
            }
        }

        i += 1;
    }

    blocks
}

/// Find a block by alias in a list of parsed blocks.
pub fn find_block_by_alias<'a>(
    blocks: &'a [ParsedBlock],
    alias: &str,
) -> Option<&'a ParsedBlock> {
    blocks.iter().find(|b| b.alias.as_deref() == Some(alias))
}

/// Find all blocks above a given line position (for dependency resolution).
pub fn blocks_above(blocks: &[ParsedBlock], line: usize) -> Vec<&ParsedBlock> {
    blocks.iter().filter(|b| b.line_start < line).collect()
}

/// Strip the opening fence and return the info string, if any.
/// Supports ``` and ~~~.
fn strip_fence_open(line: &str) -> Option<&str> {
    if line.starts_with("```") && line.len() > 3 {
        Some(line[3..].trim())
    } else if line.starts_with("~~~") && line.len() > 3 {
        Some(line[3..].trim())
    } else {
        None
    }
}

/// Parse info string into block_type and key=value attributes.
/// Example: "http alias=login displayMode=split" -> ("http", {"alias": "login", "displayMode": "split"})
fn parse_info_string(info: &str) -> (String, HashMap<String, String>) {
    let mut parts = info.split_whitespace();
    let block_type = parts.next().unwrap_or("").to_string();
    let mut attrs = HashMap::new();

    for part in parts {
        if let Some((key, value)) = part.split_once('=') {
            attrs.insert(key.to_string(), value.to_string());
        }
    }

    (block_type, attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_block() {
        let md = r#"# My API Doc

Some text here.

```http alias=login displayMode=split
{"method":"POST","url":"https://api.test.com/login","params":[],"headers":[],"body":"{\"user\":\"admin\"}"}
```

More text.
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "http");
        assert_eq!(blocks[0].alias.as_deref(), Some("login"));
        assert_eq!(blocks[0].display_mode.as_deref(), Some("split"));
        assert_eq!(blocks[0].params["method"], "POST");
        assert_eq!(blocks[0].params["url"], "https://api.test.com/login");
        assert_eq!(blocks[0].line_start, 4);
        assert_eq!(blocks[0].line_end, 6);
    }

    #[test]
    fn test_parse_db_block() {
        let md = r#"```db alias=users
{"connection_id":"abc-123","query":"SELECT * FROM users","bind_values":[]}
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "db");
        assert_eq!(blocks[0].alias.as_deref(), Some("users"));
        assert_eq!(blocks[0].params["query"], "SELECT * FROM users");
    }

    #[test]
    fn test_parse_e2e_block() {
        let md = r#"```e2e alias=flow
{"base_url":"https://api.test.com","steps":[{"name":"Step 1","method":"GET","url":"/health"}]}
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "e2e");
        assert_eq!(blocks[0].alias.as_deref(), Some("flow"));
    }

    #[test]
    fn test_parse_multiple_blocks() {
        let md = r#"# API

```http alias=auth
{"method":"POST","url":"https://api.test.com/login","params":[],"headers":[],"body":""}
```

Some text between blocks.

```http alias=users
{"method":"GET","url":"https://api.test.com/users","params":[],"headers":[],"body":""}
```

```db alias=query
{"connection_id":"1","query":"SELECT 1","bind_values":[]}
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].alias.as_deref(), Some("auth"));
        assert_eq!(blocks[1].alias.as_deref(), Some("users"));
        assert_eq!(blocks[2].alias.as_deref(), Some("query"));
    }

    #[test]
    fn test_ignores_non_executable_blocks() {
        let md = r#"```javascript
console.log("hello");
```

```http alias=req
{"method":"GET","url":"https://example.com","params":[],"headers":[],"body":""}
```

```python
print("world")
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "http");
    }

    #[test]
    fn test_find_block_by_alias() {
        let md = r#"```http alias=first
{"method":"GET","url":"https://a.com","params":[],"headers":[],"body":""}
```

```http alias=second
{"method":"GET","url":"https://b.com","params":[],"headers":[],"body":""}
```
"#;
        let blocks = parse_blocks(md);
        let found = find_block_by_alias(&blocks, "second");
        assert!(found.is_some());
        assert_eq!(found.unwrap().params["url"], "https://b.com");

        let missing = find_block_by_alias(&blocks, "nonexistent");
        assert!(missing.is_none());
    }

    #[test]
    fn test_blocks_above() {
        let md = r#"```http alias=a
{"method":"GET","url":"https://a.com","params":[],"headers":[],"body":""}
```

```http alias=b
{"method":"GET","url":"https://b.com","params":[],"headers":[],"body":""}
```

```http alias=c
{"method":"GET","url":"https://c.com","params":[],"headers":[],"body":""}
```
"#;
        let blocks = parse_blocks(md);
        let above = blocks_above(&blocks, blocks[2].line_start);
        assert_eq!(above.len(), 2);
        assert_eq!(above[0].alias.as_deref(), Some("a"));
        assert_eq!(above[1].alias.as_deref(), Some("b"));
    }

    #[test]
    fn test_block_without_alias() {
        let md = r#"```http
{"method":"GET","url":"https://example.com","params":[],"headers":[],"body":""}
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].alias.is_none());
    }

    #[test]
    fn test_invalid_json_body() {
        let md = r#"```http alias=broken
not valid json
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].params, serde_json::Value::Null);
    }

    #[test]
    fn test_db_variant_types() {
        let md = r#"```db-postgres alias=pg
{"connection_id":"1","query":"SELECT 1"}
```

```db-mysql alias=my
{"connection_id":"2","query":"SELECT 1"}
```

```db-sqlite alias=sl
{"connection_id":"3","query":"SELECT 1"}
```
"#;
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].block_type, "db-postgres");
        assert_eq!(blocks[1].block_type, "db-mysql");
        assert_eq!(blocks[2].block_type, "db-sqlite");
    }

    #[test]
    fn test_empty_markdown() {
        let blocks = parse_blocks("");
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_markdown_with_no_blocks() {
        let md = "# Title\n\nSome regular markdown text.\n\n- Item 1\n- Item 2\n";
        let blocks = parse_blocks(md);
        assert!(blocks.is_empty());
    }
}
