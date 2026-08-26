use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use std::ffi::CStr;

pgrx::pg_module_magic!();
pgrx::extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);

static MAX_FILE_BYTES: GucSetting<i32> = GucSetting::<i32>::new(4 * 1024 * 1024);
static MAX_BUNDLE_FILES: GucSetting<i32> = GucSetting::<i32>::new(100_000);
static MAX_FRONTMATTER_BYTES: GucSetting<i32> = GucSetting::<i32>::new(256 * 1024);
static MAX_GRAPH_HOPS: GucSetting<i32> = GucSetting::<i32>::new(5);
static LOG_LEVEL: GucSetting<Option<&'static CStr>> =
    GucSetting::<Option<&'static CStr>>::new(Some(c"warning"));

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_int_guc(
        "pgokf.max_file_bytes",
        "Maximum bytes accepted for one OKF Markdown file.",
        "Hard parsing limit for files registered through pgokf.",
        &MAX_FILE_BYTES,
        1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        "pgokf.max_bundle_files",
        "Maximum number of files accepted in one OKF bundle.",
        "Hard discovery limit for bundles registered through pgokf.",
        &MAX_BUNDLE_FILES,
        1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        "pgokf.max_frontmatter_bytes",
        "Maximum bytes accepted for YAML frontmatter.",
        "Hard parsing limit for YAML frontmatter registered through pgokf.",
        &MAX_FRONTMATTER_BYTES,
        1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        "pgokf.max_graph_hops",
        "Maximum graph traversal depth.",
        "Hard limit for future pgokf graph traversal functions.",
        &MAX_GRAPH_HOPS,
        1,
        100,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        "pgokf.log_level",
        "Logging level used by pgokf.",
        "One of error, warning, notice, info, debug, or log.",
        &LOG_LEVEL,
        GucContext::Suset,
        GucFlags::default(),
    );
}

/// Confirm that the extension shared library is loaded.
#[pg_extern(schema = "pgokf", immutable, parallel_safe)]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
