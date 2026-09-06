//! Where this process looks for a configuration file.

use std::path::PathBuf;

use directories::ProjectDirs;

/// Whether to search well-known locations after `--config` and `RUSHLS_CONFIG`.
///
/// Tests pass [`Self::ExplicitOnly`] so a `rushls.toml` in the crate tree cannot
/// change a fixture that asked for compiled defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigSearch {
    ExplicitOnly,
    WellKnown,
}

/// Platform project directories for this binary.
///
/// `None` when the process has no home directory.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "crowdcast", "rushls")
}

/// Places a configuration file may live, first match wins.
///
/// `--config` and `RUSHLS_CONFIG` are not in this list: those are explicit and
/// fail if the path is missing. These are discovered only when neither was set.
pub fn well_known_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("rushls.toml"));
    }
    if let Some(dirs) = project_dirs() {
        push_unique(&mut paths, dirs.config_local_dir().join("rushls.toml"));
        push_unique(&mut paths, dirs.config_dir().join("rushls.toml"));
    }
    paths
}

/// First existing file in `candidates`, or `None` if none are present.
pub fn first_existing_file(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_file())
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if paths.iter().any(|existing| existing == &path) {
        return;
    }
    paths.push(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_paths_start_with_the_working_directory() -> Result<(), std::io::Error> {
        let paths = well_known_config_paths();
        let cwd = std::env::current_dir()?;
        assert_eq!(paths.first(), Some(&cwd.join("rushls.toml")));
        if let Some(dirs) = project_dirs() {
            assert!(
                paths.contains(&dirs.config_local_dir().join("rushls.toml")),
                "local config directory is searched after the working directory"
            );
            assert!(
                paths.contains(&dirs.config_dir().join("rushls.toml")),
                "platform config directory is searched last"
            );
        }
        Ok(())
    }

    #[test]
    fn first_existing_file_skips_missing_candidates() -> Result<(), std::io::Error> {
        let missing = std::env::temp_dir().join("rushls-config-missing.toml");
        let present =
            std::env::temp_dir().join(format!("rushls-config-present-{}.toml", std::process::id()));
        std::fs::write(&present, "publishers = 1\n")?;
        let found = first_existing_file([missing, present.clone()]);
        assert_eq!(found.as_deref(), Some(present.as_path()));
        let _ = std::fs::remove_file(present);
        Ok(())
    }
}
