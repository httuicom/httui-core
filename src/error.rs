use crate::executor::ExecutorError;
use crate::runner::RunnerError;

/// Top-level error type for the core library.
///
/// Wraps the more specific errors raised by submodules (parser, runner,
/// executor, db, fs) under a single category surface so consumers —
/// desktop, TUI, MCP — can match on category and convert to whatever
/// shape their own boundary needs (Tauri IPC strings, ratatui status bar,
/// JSON-RPC payload).
///
/// Existing public APIs that already return tighter error types
/// ([`RunnerError`], [`ExecutorError`]) keep their signatures; the `From`
/// impls let consumers `?`-bubble them into [`CoreError`] without
/// touching legacy callsites.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("parse error at line {line}: {message}")]
    Parse { message: String, line: usize },

    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),

    #[error("executor error: {0}")]
    Executor(#[from] ExecutorError),

    #[error("database error: {0}")]
    Db(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("reference resolution failed: {0}")]
    References(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

/// Result alias used across the core for new APIs (existing tighter
/// `Result<T, ExecutorError>` etc. continue working unchanged).
pub type CoreResult<T> = Result<T, CoreError>;

impl From<sqlx::Error> for CoreError {
    fn from(e: sqlx::Error) -> Self {
        CoreError::Db(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_runner() -> CoreResult<()> {
        Err(RunnerError::BlockNotFound("x".into()))?;
        Ok(())
    }

    fn from_executor() -> CoreResult<()> {
        Err(ExecutorError("boom".into()))?;
        Ok(())
    }

    fn from_io() -> CoreResult<()> {
        Err(std::io::Error::other("disk full"))?;
        Ok(())
    }

    fn from_serde() -> CoreResult<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str("not json")?;
        Ok(v)
    }

    #[test]
    fn runner_bubbles() {
        let err = from_runner().unwrap_err();
        assert!(matches!(err, CoreError::Runner(_)));
        assert!(err.to_string().contains("Block 'x' not found"));
    }

    #[test]
    fn executor_bubbles() {
        let err = from_executor().unwrap_err();
        assert!(matches!(err, CoreError::Executor(_)));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn io_bubbles() {
        let err = from_io().unwrap_err();
        assert!(matches!(err, CoreError::Io(_)));
    }

    #[test]
    fn serde_bubbles() {
        let err = from_serde().unwrap_err();
        assert!(matches!(err, CoreError::Serde(_)));
    }

    #[test]
    fn db_from_sqlx() {
        let sqlx_err = sqlx::Error::PoolTimedOut;
        let core_err: CoreError = sqlx_err.into();
        assert!(matches!(core_err, CoreError::Db(_)));
    }
}
