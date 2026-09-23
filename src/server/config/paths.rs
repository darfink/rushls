//! Where this process looks for a configuration file, and where spilled media
//! lands when the operator did not name a directory.

use std::path::PathBuf;

use directories::ProjectDirs;

/// Platform project directories for this binary.
///
/// `None` when the process has no home directory, which is why `capacity.dir`
/// remains the escape hatch for a disk tier in that environment.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "rushls")
}

/// Default overflow directory: the platform cache, not durable data.
///
/// The window is process-lifetime. Putting it under the cache directory says
/// that, rather than implying a catalog that survives reboot.
pub fn default_disk_directory() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.cache_dir().join("dvr"))
}

/// True when `path` is under the platform cache directory.
#[cfg(test)]
pub fn is_under_cache_dir(path: &std::path::Path) -> bool {
    project_dirs().is_some_and(|dirs| path.starts_with(dirs.cache_dir()))
}
