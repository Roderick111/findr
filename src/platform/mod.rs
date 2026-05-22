//! Platform abstraction layer.
//!
//! Each platform (macOS, Linux, Windows) provides identically-named free functions
//! selected at compile time via `#[cfg]`. Zero runtime cost.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(target_os = "macos"))]
pub mod ocr_engine;

/// Normalize path string to forward slashes for consistent pattern matching.
/// No-op on Unix (paths already use `/`). On Windows, replaces `\` with `/`.
#[cfg(target_os = "windows")]
pub fn normalize_path_str(path: &str) -> std::borrow::Cow<'_, str> {
    if path.contains('\\') {
        std::borrow::Cow::Owned(path.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(path)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn normalize_path_str(path: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(path)
}
