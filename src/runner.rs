use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::db::connections::PoolManager;
use crate::db::environments;
use crate::executor::{BlockRequest, BlockResult, ExecutorRegistry};
use crate::parser::{self, ParsedBlock};
use crate::references;

/// Errors from the block runner.
#[derive(Debug)]
pub enum RunnerError {
    NoteRead(String),
    BlockNotFound(String),
    InvalidParams(String),
    CyclicDependency(String),
    DependencyFailed(String),
    ExecutionFailed(String),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoteRead(e) => write!(f, "Failed to read note: {e}"),
            Self::BlockNotFound(alias) => write!(f, "Block '{alias}' not found"),
            Self::InvalidParams(e) => write!(f, "Invalid block params: {e}"),
            Self::CyclicDependency(alias) => write!(f, "Cyclic dependency on '{alias}'"),
            Self::DependencyFailed(e) => write!(f, "Dependency failed: {e}"),
            Self::ExecutionFailed(e) => write!(f, "Execution failed: {e}"),
        }
    }
}

impl std::error::Error for RunnerError {}

/// Executes blocks from raw markdown with full dependency and environment resolution.
pub struct BlockRunner {
    registry: Arc<ExecutorRegistry>,
    pool: sqlx::sqlite::SqlitePool,
    conn_manager: Arc<PoolManager>,
}

impl BlockRunner {
    pub fn new(
        registry: Arc<ExecutorRegistry>,
        pool: sqlx::sqlite::SqlitePool,
        conn_manager: Arc<PoolManager>,
    ) -> Self {
        Self {
            registry,
            pool,
            conn_manager,
        }
    }

    /// Execute a block by alias from a note file.
    ///
    /// 1. Read the note and parse all blocks
    /// 2. Find the target block by alias
    /// 3. Resolve dependencies (execute referenced blocks first)
    /// 4. Resolve environment variables
    /// 5. Execute the block
    pub async fn execute(
        &self,
        vault_path: &str,
        note_path: &str,
        alias: &str,
    ) -> Result<BlockResult, RunnerError> {
        // Read and parse
        let content = crate::fs::read_note(vault_path, note_path)
            .map_err(RunnerError::NoteRead)?;
        let blocks = parser::parse_blocks(&content);

        // Load environment variables
        let env_vars = self.load_active_env_vars().await;

        // Execute with dependency resolution
        let mut executed: HashMap<String, serde_json::Value> = HashMap::new();
        let mut in_progress: HashSet<String> = HashSet::new();

        self.execute_block_recursive(
            alias,
            &blocks,
            &env_vars,
            &mut executed,
            &mut in_progress,
        )
        .await
    }

    /// Recursively execute a block, first executing any dependencies.
    fn execute_block_recursive<'a>(
        &'a self,
        alias: &'a str,
        blocks: &'a [ParsedBlock],
        env_vars: &'a HashMap<String, String>,
        executed: &'a mut HashMap<String, serde_json::Value>,
        in_progress: &'a mut HashSet<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<BlockResult, RunnerError>> + Send + 'a>> {
        Box::pin(async move {
        // Already executed? Return cached result
        if let Some(data) = executed.get(alias) {
            return Ok(BlockResult {
                status: "success".to_string(),
                data: data.clone(),
                duration_ms: 0,
            });
        }

        // Cycle detection
        if in_progress.contains(alias) {
            return Err(RunnerError::CyclicDependency(alias.to_string()));
        }
        in_progress.insert(alias.to_string());

        // Find the block
        let block = parser::find_block_by_alias(blocks, alias)
            .ok_or_else(|| RunnerError::BlockNotFound(alias.to_string()))?;

        if block.params.is_null() {
            return Err(RunnerError::InvalidParams(
                "Block has no valid JSON parameters".to_string(),
            ));
        }

        // Find blocks above this one (for dependency resolution)
        let above = parser::blocks_above(blocks, block.line_start);

        // Extract placeholders and resolve dependencies
        let placeholders = references::extract_placeholders(&block.params);
        for placeholder in &placeholders {
            if references::is_block_reference(placeholder) {
                if let Some(dep_alias) = references::extract_alias(placeholder) {
                    // Only execute if it's a block above us
                    if above.iter().any(|b| b.alias.as_deref() == Some(dep_alias))
                        && !executed.contains_key(dep_alias)
                    {
                        self.execute_block_recursive(
                            dep_alias,
                            blocks,
                            env_vars,
                            executed,
                            in_progress,
                        )
                        .await
                        .map_err(|e| {
                            RunnerError::DependencyFailed(format!("{dep_alias}: {e}"))
                        })?;
                    }
                }
            }
        }

        // Resolve all placeholders in params
        let resolved_params = references::resolve_all(&block.params, executed, env_vars);

        // Normalize block_type
        let block_type = if block.block_type.starts_with("db-") || block.block_type == "db" {
            "db".to_string()
        } else {
            block.block_type.clone()
        };

        // Execute
        let req = BlockRequest {
            block_type,
            params: resolved_params,
        };

        let result = self
            .registry
            .execute(req)
            .await
            .map_err(|e| RunnerError::ExecutionFailed(e.to_string()))?;

        // Cache the result data for downstream blocks
        executed.insert(alias.to_string(), result.data.clone());
        in_progress.remove(alias);

        Ok(result)
        }) // Box::pin
    }

    /// Load variables from the currently active environment.
    async fn load_active_env_vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        let envs = match environments::list_environments(&self.pool).await {
            Ok(e) => e,
            Err(_) => return vars,
        };

        let active = match envs.iter().find(|e| e.is_active) {
            Some(e) => e,
            None => return vars,
        };

        if let Ok(env_vars) = environments::list_env_variables(&self.pool, &active.id).await {
            for var in env_vars {
                vars.insert(var.key, var.value);
            }
        }

        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::executor::http::HttpExecutor;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, BlockRunner, String) {
        let tmp = TempDir::new().unwrap();
        let pool = db::init_db(tmp.path()).await.unwrap();
        let conn_manager = Arc::new(PoolManager::new_standalone(pool.clone()));

        let mut registry = ExecutorRegistry::new();
        registry.register(Box::new(HttpExecutor::new()));

        let runner = BlockRunner::new(Arc::new(registry), pool, conn_manager);

        // Create vault dir
        let vault_dir = tmp.path().join("vault");
        std::fs::create_dir_all(&vault_dir).unwrap();
        let vault_path = vault_dir.to_string_lossy().to_string();

        (tmp, runner, vault_path)
    }

    #[tokio::test]
    async fn test_block_not_found() {
        let (_tmp, runner, vault_path) = setup().await;

        // Create a note with no blocks
        std::fs::write(
            format!("{}/test.md", vault_path),
            "# No blocks here\n",
        )
        .unwrap();

        let result = runner.execute(&vault_path, "test.md", "missing").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RunnerError::BlockNotFound(_)));
    }

    #[tokio::test]
    async fn test_note_not_found() {
        let (_tmp, runner, vault_path) = setup().await;

        let result = runner.execute(&vault_path, "nonexistent.md", "alias").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RunnerError::NoteRead(_)));
    }

    #[tokio::test]
    async fn test_execute_simple_http_block() {
        let (_tmp, runner, vault_path) = setup().await;

        // Start a mock server
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/health"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"status": "ok"}),
            ))
            .mount(&server)
            .await;

        let note_content = format!(
            r#"# Health Check

```http alias=health
{{"method":"GET","url":"{}/health","params":[],"headers":[],"body":""}}
```
"#,
            server.uri()
        );

        std::fs::write(format!("{}/health.md", vault_path), note_content).unwrap();

        let result = runner.execute(&vault_path, "health.md", "health").await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["status_code"], 200);
        assert_eq!(result.data["body"]["status"], "ok");
    }

    #[tokio::test]
    async fn test_env_variable_resolution() {
        let (_tmp, runner, vault_path) = setup().await;

        // Create environment with variable
        let env = crate::db::environments::create_environment(&runner.pool, "test".to_string())
            .await
            .unwrap();
        crate::db::environments::set_active_environment(&runner.pool, Some(&env.id))
            .await
            .unwrap();

        let server = wiremock::MockServer::start().await;
        crate::db::environments::set_env_variable(
            &runner.pool,
            &env.id,
            "API_URL".to_string(),
            server.uri(),
            false,
        )
        .await
        .unwrap();

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true})),
            )
            .mount(&server)
            .await;

        let note_content = r#"# API Test

```http alias=api
{"method":"GET","url":"{{API_URL}}/api","params":[],"headers":[],"body":""}
```
"#;

        std::fs::write(format!("{}/api.md", vault_path), note_content).unwrap();

        let result = runner.execute(&vault_path, "api.md", "api").await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.data["body"]["ok"], true);
    }
}
