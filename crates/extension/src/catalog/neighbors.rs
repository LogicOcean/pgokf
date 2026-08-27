//! Graph-traversal seam (recursive neighbor queries).
//!
//! # Seam contract for the neighbors feature wave
//!
//! This module is intentionally empty until the links wave has populated
//! `pgokf.links` (see [`crate::catalog::links`]). The wave that fills it
//! should add the SQL-facing traversal API here — for example
//! `pgokf.concept_neighbors(bundle_id, concept_id, max_hops)` — inside its
//! own `#[pgrx::pg_schema] mod pgokf { ... }` block, without touching the
//! sync engine:
//!
//! - traversals must be cycle-safe recursive CTEs, bundle-scoped, and
//!   depth-limited by [`crate::guc::max_graph_hops`];
//! - authorization is reader-level via
//!   [`crate::security::authorize_current_user`] with
//!   [`crate::security::Operation::Search`];
//! - order its SQL after the links wave's table with `requires = [...]` on
//!   that wave's named block;
//! - external and unresolved link rows never become traversal edges.
