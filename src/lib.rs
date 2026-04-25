pub mod block_history;
pub mod block_results;
pub mod block_settings;
pub mod blocks;
pub mod config;
pub mod db;
pub mod error;
pub mod executor;
pub mod fs;
pub mod paths;
pub mod references;
pub mod runner;
pub mod search;
pub mod vaults;

pub use error::{CoreError, CoreResult};

// Compat re-export: external consumers (`httui-mcp`) historically imported
// `httui_core::parser`. The module moved under `blocks::parser`; the alias
// keeps the old path working until those crates migrate.
pub use blocks::parser;
