//! Named semantic assertions distinguish intended rejection from setup panics.
#[macro_export]
macro_rules! semantic_assert {
    ($marker:literal, $condition:expr $(,)?) => {
        assert!($condition, "PGOKF_ASSERT:{}:{}", $marker, stringify!($condition));
    };
    ($marker:literal, $condition:expr, $($message:tt)+) => {
        assert!($condition, "PGOKF_ASSERT:{}:{}", $marker, format_args!($($message)+));
    };
}
