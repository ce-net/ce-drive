//! OS-independent ProjFS decision logic — the parts of the Windows ProjFS adapter that do **not**
//! touch the Win32 API, so they compile and are unit-tested on **every** host (the `cfg(windows)`
//! ProjFS callbacks in [`crate::windows`] are a thin FFI shell around these).
//!
//! This mirrors the discipline [`crate::winnames`] uses for path-component name rules: the valuable,
//! testable logic lives in an always-compiled module, and the `cfg(windows)`-gated FFI shell calls
//! into it. Here that logic is (1) mapping a Win32 backslash path to the engine's slash path with
//! per-component §7.4 normalization, and (2) deriving the numeric fields of a ProjFS
//! `PRJ_FILE_BASIC_INFO` (size / directory flag / FILETIME / FILE_ATTRIBUTE_*) from an engine
//! [`Attr`](crate::vfs::Attr). Both are shared with the WinFsp adapter so there is one tested copy.

use crate::vfs::Attr;

/// The numeric fields a ProjFS `PRJ_FILE_BASIC_INFO` needs, derived from an engine [`Attr`]. The
/// `cfg(windows)` callback copies these into the real `PRJ_FILE_BASIC_INFO` struct; keeping them in a
/// plain POD here lets the mapping be unit-tested with no Windows headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicInfoFields {
    /// True for a directory (ProjFS `IsDirectory`).
    pub is_directory: bool,
    /// File size in bytes (0 for directories).
    pub file_size: u64,
    /// Windows FILETIME: 100ns ticks since 1601-01-01 (used for all four ProjFS timestamps).
    pub filetime: u64,
    /// FILE_ATTRIBUTE_* bitmask (DIRECTORY for dirs, NORMAL for files).
    pub file_attributes: u32,
}

/// FILE_ATTRIBUTE_DIRECTORY.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// FILE_ATTRIBUTE_NORMAL.
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// 1601→1970 epoch difference in 100ns ticks (the Windows/Unix epoch gap).
const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;

/// Convert engine unix-ms into a Windows FILETIME (100ns ticks since 1601). Saturating so a
/// hostile/overflowing mtime can never panic inside the FFI callback path (no `unwrap`, no overflow).
pub fn ms_to_filetime(mtime_ms: u64) -> u64 {
    EPOCH_DIFF_100NS.saturating_add(mtime_ms.saturating_mul(10_000))
}

/// Build the ProjFS basic-info fields from an engine [`Attr`].
pub fn basic_info_fields(a: &Attr) -> BasicInfoFields {
    BasicInfoFields {
        is_directory: a.is_dir,
        file_size: a.size,
        filetime: ms_to_filetime(a.mtime_ms),
        file_attributes: if a.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL },
    }
}

/// Map a Win32 backslash path (what ProjFS hands the callbacks) to the engine's slash-separated
/// absolute path, normalizing each component (reserved names, forbidden chars, trailing dot/space)
/// per §7.4. `"\\"`/`""` is the root. Returns `None` if any component is unrepresentable on Windows
/// (so the callback can return ERROR_FILE_NOT_FOUND rather than project an illegal entry). Identical
/// contract to the WinFsp adapter's mapper; factored here so both share one tested copy.
pub fn win_path_to_engine(p: &str) -> Option<String> {
    if p == "\\" || p.is_empty() {
        return Some("/".to_string());
    }
    let mut out = String::new();
    for comp in p.split('\\').filter(|c| !c.is_empty()) {
        out.push('/');
        out.push_str(&crate::winnames::normalize_windows(comp)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Attr;

    #[test]
    fn win_path_maps_to_engine_path() {
        assert_eq!(win_path_to_engine("\\"), Some("/".to_string()));
        assert_eq!(win_path_to_engine(""), Some("/".to_string()));
        assert_eq!(win_path_to_engine("\\src\\main.rs"), Some("/src/main.rs".to_string()));
        assert_eq!(win_path_to_engine("\\docs"), Some("/docs".to_string()));
        // A reserved component is rejected (Windows can't represent it).
        assert_eq!(win_path_to_engine("\\src\\CON"), None);
        assert_eq!(win_path_to_engine("\\nul.txt"), None);
        // Forbidden char rejected.
        assert_eq!(win_path_to_engine("\\a:b"), None);
        assert_eq!(win_path_to_engine("\\name "), None, "trailing space rejected");
    }

    #[test]
    fn basic_info_marks_dirs_and_files() {
        let dir = Attr { ino: 1, size: 0, mode: 0o755, mtime_ms: 0, is_dir: true };
        let file = Attr { ino: 2, size: 42, mode: 0o644, mtime_ms: 1_000, is_dir: false };
        let d = basic_info_fields(&dir);
        assert!(d.is_directory);
        assert_eq!(d.file_size, 0);
        assert_eq!(d.file_attributes, FILE_ATTRIBUTE_DIRECTORY);
        let f = basic_info_fields(&file);
        assert!(!f.is_directory);
        assert_eq!(f.file_size, 42);
        assert_eq!(f.file_attributes, FILE_ATTRIBUTE_NORMAL);
        // mtime 1000 ms => epoch diff + 10_000_000 ticks (1000 * 10_000).
        assert_eq!(f.filetime, EPOCH_DIFF_100NS + 10_000_000);
    }

    #[test]
    fn filetime_saturates_on_overflow() {
        // Must never panic in the FFI callback path — saturating arithmetic, not wrapping/panicking.
        assert_eq!(ms_to_filetime(u64::MAX), u64::MAX);
        assert_eq!(ms_to_filetime(0), EPOCH_DIFF_100NS);
    }
}
