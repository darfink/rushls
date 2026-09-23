//! Filesystem guarantees shared by recording and retained media.
use cap_std::fs::Dir;
use std::{ffi::OsStr, io};

/// Windows cannot flush directory handles through the portable file API.
/// File contents are synced before publication on every platform; Unix also
/// syncs directory entries. Windows power-loss metadata durability is not promised.
pub fn sync_directory(directory: &Dir) -> io::Result<()> {
    #[cfg(unix)]
    {
        // Linux directory capabilities may use O_PATH. Opening "." relative
        // to the capability obtains a readable directory handle for fsync.
        directory.open(".")?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = directory;
        Ok(())
    }
}

/// Reject Windows aliases and alternate data streams even on Unix, so an
/// archive name has the same meaning when moved between supported platforms.
pub fn safe_component(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ' '])
        .to_ascii_uppercase();
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem.ends_with(['1', '2', '3', '4', '5', '6', '7', '8', '9', '¹', '²', '³']))
}
