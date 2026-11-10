//! Identity-bound process signaling on macOS.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{CoalitionHandle, CoalitionId, ProcessHandle, SignalResult};
