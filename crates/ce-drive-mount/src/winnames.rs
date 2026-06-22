//! Windows/macOS path-component normalization — the design's §7.4 checklist, as a **pure,
//! platform-independent** function so it is unit-tested on every host (the Windows adapter itself is
//! `cfg(windows)`-gated and cannot run on the macOS/Linux CI lanes, but its name rules must still be
//! verified there).
//!
//! Windows rejects a set of reserved device names, reserved characters, and trailing dot/space; a
//! CE drive authored on a case-sensitive, less-restrictive platform (Linux) must not present
//! filesystem-illegal entries when mounted on Windows. We normalize at the adapter boundary.

/// Windows reserved device names (case-insensitive, with or without an extension).
pub const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters Win32 forbids in a file/directory name.
pub const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*', '\0'];

/// Validate/normalize one Windows path component. Returns `None` for names Windows cannot represent:
/// empty, `.`/`..`, containing a forbidden char or a control char, ending in a dot or space (Win32
/// strips those, breaking identity), or a reserved device name (with or without an extension).
pub fn normalize_windows(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if name.chars().any(|c| FORBIDDEN.contains(&c) || (c as u32) < 0x20) {
        return None;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return None;
    }
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return None;
    }
    Some(name.to_string())
}

/// Validate one macOS path component. macOS is far more permissive than Windows (only `/` and NUL
/// are illegal, plus `.`/`..`/empty), so this rejects only the structurally-impossible cases. Case
/// folding and NFD normalization are handled by the kernel; we preserve authoring bytes.
pub fn normalize_macos(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_rejects_reserved_and_illegal() {
        assert_eq!(normalize_windows("a.txt"), Some("a.txt".to_string()));
        assert_eq!(normalize_windows("Cargo.toml"), Some("Cargo.toml".to_string()));
        // Reserved device names, with or without extension, any case.
        assert!(normalize_windows("CON").is_none());
        assert!(normalize_windows("con.txt").is_none());
        assert!(normalize_windows("Aux").is_none());
        assert!(normalize_windows("COM1").is_none());
        assert!(normalize_windows("lpt9.log").is_none());
        // Forbidden chars.
        assert!(normalize_windows("a:b").is_none());
        assert!(normalize_windows("a*b").is_none());
        assert!(normalize_windows("a/b").is_none());
        assert!(normalize_windows("a\\b").is_none());
        // Trailing dot/space.
        assert!(normalize_windows("name.").is_none());
        assert!(normalize_windows("name ").is_none());
        // Structural.
        assert!(normalize_windows("").is_none());
        assert!(normalize_windows(".").is_none());
        assert!(normalize_windows("..").is_none());
        // A name that merely *contains* a reserved stem segment is fine (only the first stem matters).
        assert_eq!(normalize_windows("console.js"), Some("console.js".to_string()));
    }

    #[test]
    fn macos_rejects_only_structural() {
        assert_eq!(normalize_macos("a.txt"), Some("a.txt".to_string()));
        assert_eq!(normalize_macos("CON"), Some("CON".to_string()), "reserved on win, legal on mac");
        assert_eq!(normalize_macos("a:b"), Some("a:b".to_string()), "colon legal on mac");
        assert!(normalize_macos("a/b").is_none());
        assert!(normalize_macos("").is_none());
        assert!(normalize_macos(".").is_none());
        assert!(normalize_macos("..").is_none());
    }
}
