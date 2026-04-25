//! Block parsing, serialization, and type registry.
//!
//! Public surface for working with executable code blocks embedded in
//! markdown notes. Submodules:
//!
//! - [`parser`] — markdown → [`ParsedBlock`]
//! - [`serializer`] — [`ParsedBlock`] → fenced markdown (canonical, deterministic)
//! - [`registry`] — alias mapping for block-type → executor names

pub mod parser;
pub mod registry;
pub mod serializer;

pub use parser::{blocks_above, find_block_by_alias, parse_blocks, ParsedBlock};
pub use registry::BlockTypeRegistry;
pub use serializer::serialize_block;
