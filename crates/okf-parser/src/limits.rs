/// Resource limits applied before parsing untrusted concept files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserLimits {
    pub max_file_bytes: usize,
    pub max_frontmatter_bytes: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 4 * 1024 * 1024,
            max_frontmatter_bytes: 256 * 1024,
        }
    }
}
