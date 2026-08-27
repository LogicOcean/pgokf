//! Configuration seam (`allowed_roots` and friends).
//!
//! # Seam contract for the config feature wave
//!
//! This module is intentionally empty. The wave that fills it owns the
//! persistent configuration surface (for example a `pgokf_private` settings
//! table plus admin functions to manage allowed bundle roots) and should:
//!
//! - create its storage in an `extension_sql!` block with
//!   `requires = ["catalog_tables"]`;
//! - expose a lookup such as `allowed_roots() -> Vec<PathBuf>` and tighten
//!   bundle registration by routing root resolution through
//!   [`crate::security::canonicalize_contained_path`] (symlink-escape-safe
//!   containment) once roots are configured. Until then, the sync engine
//!   documents and applies the interim policy: any absolute, canonical,
//!   traversal-free path is accepted, and registration remains restricted
//!   to `pgokf_admin` (see [`crate::catalog::sync`], "Path containment");
//! - keep hard resource ceilings in the `pgokf.*` GUCs ([`crate::guc`]);
//!   this module is for cluster-persistent catalog policy, not per-session
//!   settings.
