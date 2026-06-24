//! File/directory name validation — the cross-platform name discipline enforced at every structural
//! mutation (`mkdir`, `add_file`, `mv`/rename).
//!
//! A drive name is a single path *component*: it is joined with `/` to derive paths and split on `/`
//! to resolve them. A name that itself contains `/` (or is `.`/`..`/empty/NUL) silently corrupts path
//! derivation and resolution — a `/`-bearing name makes a node unreachable, and `..` enables scope
//! escape against the share caveat. The Windows adapter has its own reserved-name normalization
//! ([`crate::winnames`] in the mount crate); this module is the *portable, always-enforced* floor that
//! every writer (single- and multi-writer, CLI and mount) shares.
//!
//! The rules (a deliberately strict, portable subset):
//! * non-empty, and not pure whitespace,
//! * not `.` or `..` (path-relative components are never valid node names),
//! * no `/` or `\\` (path separators on any host),
//! * no NUL or other ASCII control characters (`< 0x20`),
//! * at most [`MAX_NAME_BYTES`] UTF-8 bytes (the ext4/most-FS component limit).
//!
//! These are validated once, up front, returning a precise [`NameError`]; downstream code can then
//! assume every stored edge name is a clean single component.

use std::fmt;

/// The maximum length of a single name component, in UTF-8 bytes. 255 is the de-facto floor across
/// ext4, APFS, and NTFS (which count UTF-16 code units, but 255 bytes is always safe).
pub const MAX_NAME_BYTES: usize = 255;

/// Why a name was rejected. A typed error so callers (and tests) can match the exact failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    /// The name was empty or only whitespace.
    Empty,
    /// The name was `.` or `..` (a relative path component, never a node name).
    DotComponent,
    /// The name contained a path separator (`/` or `\\`).
    Separator(char),
    /// The name contained a NUL or other ASCII control character.
    Control(char),
    /// The name exceeded [`MAX_NAME_BYTES`] UTF-8 bytes.
    TooLong { bytes: usize },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => write!(f, "name is empty"),
            NameError::DotComponent => write!(f, "name '.' or '..' is not a valid file name"),
            NameError::Separator(c) => write!(f, "name contains a path separator '{c}'"),
            NameError::Control(c) => {
                write!(f, "name contains a control character U+{:04X}", *c as u32)
            }
            NameError::TooLong { bytes } => {
                write!(f, "name is {bytes} bytes, exceeds the {MAX_NAME_BYTES}-byte limit")
            }
        }
    }
}

impl std::error::Error for NameError {}

/// Validate a single name component. `Ok(())` if it is a clean, portable, single path component.
///
/// ```
/// use ce_drive_core::names::{validate_name, NameError};
/// assert!(validate_name("report.pdf").is_ok());
/// assert_eq!(validate_name(""), Err(NameError::Empty));
/// assert_eq!(validate_name(".."), Err(NameError::DotComponent));
/// assert_eq!(validate_name("a/b"), Err(NameError::Separator('/')));
/// assert!(validate_name("a\0b").is_err());
/// ```
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.trim().is_empty() {
        return Err(NameError::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameError::DotComponent);
    }
    for c in name.chars() {
        if c == '/' || c == '\\' {
            return Err(NameError::Separator(c));
        }
        if (c as u32) < 0x20 {
            return Err(NameError::Control(c));
        }
    }
    let bytes = name.len();
    if bytes > MAX_NAME_BYTES {
        return Err(NameError::TooLong { bytes });
    }
    Ok(())
}

/// Validate a name, returning an `anyhow::Error` (the form the [`crate::drive::Drive`] API uses).
pub fn check_name(name: &str) -> anyhow::Result<()> {
    validate_name(name).map_err(|e| anyhow::anyhow!("invalid name '{name}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for n in ["a", "report.pdf", "My Folder", "файл.txt", "a.b.c", "x-1_2", "café"] {
            assert!(validate_name(n).is_ok(), "{n} should be valid");
        }
    }

    #[test]
    fn rejects_empty_and_whitespace() {
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name("   "), Err(NameError::Empty));
        // A lone tab trims to empty, so it is reported as Empty.
        assert_eq!(validate_name("\t"), Err(NameError::Empty));
        // A control char embedded between real chars is reported as Control.
        assert_eq!(validate_name("a\tb"), Err(NameError::Control('\t')));
    }

    #[test]
    fn rejects_dot_components() {
        assert_eq!(validate_name("."), Err(NameError::DotComponent));
        assert_eq!(validate_name(".."), Err(NameError::DotComponent));
        // A leading-dot name like ".gitignore" is fine (it is not the bare `.`/`..`).
        assert!(validate_name(".gitignore").is_ok());
        assert!(validate_name("...").is_ok());
    }

    #[test]
    fn rejects_separators() {
        assert_eq!(validate_name("a/b"), Err(NameError::Separator('/')));
        assert_eq!(validate_name("a\\b"), Err(NameError::Separator('\\')));
        assert_eq!(validate_name("/"), Err(NameError::Separator('/')));
    }

    #[test]
    fn rejects_control_chars() {
        assert_eq!(validate_name("a\0b"), Err(NameError::Control('\0')));
        assert_eq!(validate_name("a\nb"), Err(NameError::Control('\n')));
        assert_eq!(validate_name("a\rb"), Err(NameError::Control('\r')));
    }

    #[test]
    fn rejects_overlong() {
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(validate_name(&long), Err(NameError::TooLong { bytes: MAX_NAME_BYTES + 1 }));
        // Exactly at the limit is OK.
        assert!(validate_name(&"y".repeat(MAX_NAME_BYTES)).is_ok());
    }

    #[test]
    fn check_name_wraps_in_anyhow() {
        let e = check_name("a/b").unwrap_err();
        assert!(e.to_string().contains("invalid name"));
        assert!(e.to_string().contains("separator"));
    }
}
