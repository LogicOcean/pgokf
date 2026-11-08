//! Deterministic verification primitives shared by repository tests and the CLI.
pub mod artifact_checks;
pub mod catalog;
pub mod checkout_shapes;
pub mod corpus;
pub mod historical;
pub mod homebrew_observation;
pub mod image_smoke;
pub mod json;
pub mod mcp;
pub mod mcp_coverage;
pub mod migration;
pub mod mutations;
pub mod owned_docker;
pub mod ownership;
pub mod package_archive;
pub mod policy_campaign;
pub mod positional;
pub mod postgres_pin;
pub mod process;
pub mod provider;
pub mod reconciliation;
pub mod reconciliation_mutations;
pub mod release;
pub mod release_identity;
pub mod release_runtime;
pub mod source;
pub mod sql;
pub mod upgrade;
pub mod workflow;

#[cfg(unix)]
pub mod service;

#[cfg(test)]
mod semantic;

mod scratch;
