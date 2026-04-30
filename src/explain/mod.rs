//! Unified `PlanNode` tree for `EXPLAIN ANALYZE` output across
//! drivers. Story 02 of Epic 53 ships the Postgres parser first
//! (most complete; canvas mock targets it). MySQL and MongoDB
//! parsers carry to follow-up slices.
//!
//! The tree is the surface the React `<ExplainPlan>` component
//! consumes; the per-driver parsers are responsible for translating
//! each driver's JSON shape into the same `PlanNode` shape so the
//! UI doesn't fan out per backend.

pub mod mongo;
pub mod mysql;
pub mod node;
pub mod postgres;

pub use mongo::parse_mongo_explain;
pub use mysql::parse_mysql_explain;
pub use node::PlanNode;
pub use postgres::parse_postgres_explain;
