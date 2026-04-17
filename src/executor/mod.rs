pub mod db;
pub mod e2e;
pub mod http;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct BlockRequest {
    pub block_type: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct BlockResult {
    pub status: String,
    pub data: serde_json::Value,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ExecutorError(pub String);

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ExecutorError {}

#[async_trait]
pub trait Executor: Send + Sync {
    fn block_type(&self) -> &str;
    async fn execute(&self, params: serde_json::Value) -> Result<BlockResult, ExecutorError>;
    async fn validate(&self, _params: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

pub struct ExecutorRegistry {
    executors: HashMap<String, Box<dyn Executor>>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self {
            executors: HashMap::new(),
        }
    }

    pub fn register(&mut self, executor: Box<dyn Executor>) {
        self.executors
            .insert(executor.block_type().to_string(), executor);
    }

    pub async fn execute(&self, req: BlockRequest) -> Result<BlockResult, ExecutorError> {
        let executor = self
            .executors
            .get(&req.block_type)
            .ok_or_else(|| ExecutorError(format!("Unknown block type: {}", req.block_type)))?;
        executor.validate(&req.params).await.map_err(ExecutorError)?;
        executor.execute(req.params).await
    }
}
