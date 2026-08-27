//! Catalog backbone: base tables, the shared register/refresh sync engine,
//! and native full-text search.
//!
//! # Module layout and extension seams
//!
//! The backbone is deliberately open for extension and closed for
//! modification. Core modules own the schema and the sync loop; feature
//! modules attach through fixed seams and never edit the core:
//!
//! - [`schema`] — the `catalog_tables` SQL block: base tables, indexes, and
//!   the public composite result types. Feature SQL orders itself after the
//!   base schema with `requires = ["catalog_tables"]`.
//! - [`types`] — Rust-side composite-result builders and the
//!   [`types::StagedConcept`] seam payload handed to projection steps.
//! - [`sync`] — the shared register/refresh engine. After concept rows are
//!   staged it invokes the ordered projection seam ([`links::project`], then
//!   [`provenance::project`]) before returning.
//! - [`search`] — `pgokf.concept_search` over the weighted `tsvector`.
//!
//! Feature-extension stubs, each to be filled by a later wave without
//! touching [`sync`]:
//!
//! - [`links`] — link-graph projection (`pgokf.links`).
//! - [`neighbors`] — recursive graph traversal APIs.
//! - [`provenance`] — provenance/trust/lifecycle projection.
//! - [`config`] — the `pgokf.allowed_roots` style configuration surface.
//! - [`admin`] — `bundle_info`, `unregister_bundle`, `list_bundles`.

pub mod admin;
pub mod config;
pub mod links;
pub mod neighbors;
pub mod provenance;
pub mod schema;
pub mod search;
pub mod sync;
pub mod types;

pub use types::StagedConcept;
