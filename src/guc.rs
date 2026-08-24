use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use std::ffi::{CStr, CString};

const fn cstr(bytes: &'static [u8]) -> &'static CStr {
    // SAFETY: every caller below supplies one trailing NUL and no interior NUL.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

pub const DEFAULT_MAX_FILE_BYTES: i32 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_BUNDLE_FILES: i32 = 100_000;
pub const DEFAULT_MAX_FRONTMATTER_BYTES: i32 = 256 * 1024;
pub const DEFAULT_MAX_GRAPH_HOPS: i32 = 5;
pub const DEFAULT_LOG_LEVEL: &str = "warning";

static MAX_FILE_BYTES: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_FILE_BYTES);
static MAX_BUNDLE_FILES: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_BUNDLE_FILES);
static MAX_FRONTMATTER_BYTES: GucSetting<i32> =
    GucSetting::<i32>::new(DEFAULT_MAX_FRONTMATTER_BYTES);
static MAX_GRAPH_HOPS: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_GRAPH_HOPS);
static LOG_LEVEL: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(cstr(b"warning\0")));

/// Register pgokf configuration variables.
///
/// Resource ceilings are `PGC_POSTMASTER`: they cannot be increased by a
/// session after startup. The logging setting is `PGC_SUSET`, so only a
/// superuser can alter it at runtime.
pub fn register_gucs() {
    GucRegistry::define_int_guc(
        cstr(b"pgokf.max_file_bytes\0"),
        cstr(b"Maximum bytes read from one bundle file.\0"),
        cstr(b"Hard safety limit for an individual OKF bundle file.\0"),
        &MAX_FILE_BYTES,
        1,
        i32::MAX,
        GucContext::Postmaster,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        cstr(b"pgokf.max_bundle_files\0"),
        cstr(b"Maximum files accepted in one bundle.\0"),
        cstr(b"Hard safety limit for files discovered while indexing an OKF bundle.\0"),
        &MAX_BUNDLE_FILES,
        1,
        i32::MAX,
        GucContext::Postmaster,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        cstr(b"pgokf.max_frontmatter_bytes\0"),
        cstr(b"Maximum bytes parsed as frontmatter.\0"),
        cstr(b"Hard safety limit for frontmatter parsed from one OKF document.\0"),
        &MAX_FRONTMATTER_BYTES,
        1,
        i32::MAX,
        GucContext::Postmaster,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        cstr(b"pgokf.max_graph_hops\0"),
        cstr(b"Maximum graph traversal depth.\0"),
        cstr(b"Hard safety limit for graph hops evaluated by pgokf queries.\0"),
        &MAX_GRAPH_HOPS,
        1,
        1_000,
        GucContext::Postmaster,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        cstr(b"pgokf.log_level\0"),
        cstr(b"pgokf logging threshold.\0"),
        cstr(b"Logging threshold used by pgokf; defaults to warning.\0"),
        &LOG_LEVEL,
        GucContext::Suset,
        GucFlags::default(),
    );
}

pub fn max_file_bytes() -> usize {
    MAX_FILE_BYTES.get() as usize
}

pub fn max_bundle_files() -> usize {
    MAX_BUNDLE_FILES.get() as usize
}

pub fn max_frontmatter_bytes() -> usize {
    MAX_FRONTMATTER_BYTES.get() as usize
}

pub fn max_graph_hops() -> usize {
    MAX_GRAPH_HOPS.get() as usize
}

pub fn log_level() -> String {
    LOG_LEVEL
        .get()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_limit_defaults_match_the_extension_contract() {
        assert_eq!(DEFAULT_MAX_FILE_BYTES, 4 * 1024 * 1024);
        assert_eq!(DEFAULT_MAX_BUNDLE_FILES, 100_000);
        assert_eq!(DEFAULT_MAX_FRONTMATTER_BYTES, 256 * 1024);
        assert_eq!(DEFAULT_MAX_GRAPH_HOPS, 5);
        assert_eq!(DEFAULT_LOG_LEVEL, "warning");
    }
}
