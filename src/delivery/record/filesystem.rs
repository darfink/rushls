//! Capability-relative writes keep publisher names inside the archive.
//! A hard link publishes synced contents without replacing an existing name.
use crate::{
    delivery::filesystem::{safe_component, sync_directory},
    domain::Payload,
};
use cap_fs_ext::DirExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, DirBuilder, OpenOptions},
};
use std::{
    io::{self, Write},
    path::Path,
};
use uuid::Uuid;

pub fn root(path: &Path) -> io::Result<Dir> {
    std::fs::create_dir_all(path)?;
    let absolute = std::path::absolute(path)?;
    let parent_path = absolute.parent().unwrap_or(&absolute);
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())?;
    let root = match absolute.file_name() {
        Some(name) => parent.open_dir_nofollow(name)?,
        None => parent,
    };
    // Persist newly created operator-configured ancestors on Unix. Publisher
    // paths below this root never use ambient filesystem authority.
    for ancestor in absolute.ancestors() {
        sync_directory(&Dir::open_ambient_dir(ancestor, ambient_authority())?)?;
    }
    Ok(root)
}

fn directory(parent: &Dir, name: &std::ffi::OsStr) -> io::Result<Dir> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o750);
    }
    // Keep the builder mutable on both targets without conditional bindings.
    builder.recursive(false);
    match parent.create_dir_with(name, &builder) {
        Ok(()) => sync_directory(parent)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    parent.open_dir_nofollow(name)
}

pub fn write(root: &Dir, path: &Path, payloads: &[Payload]) -> io::Result<()> {
    let mut parent = root.try_clone()?;
    let components: Vec<_> = path.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|part| !matches!(part, std::path::Component::Normal(name) if safe_component(name)))
    {
        return Err(io::Error::other(
            "archive path is not a portable relative path",
        ));
    }
    for component in &components[..components.len() - 1] {
        parent = directory(&parent, component.as_os_str())?;
    }
    let final_name = components.last().expect("nonempty path").as_os_str();
    let temporary = format!(".rushls-{}.tmp", Uuid::now_v7());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    let mut file = parent.open_with(&temporary, &options)?;
    let result = (|| {
        for payload in payloads {
            file.write_all(payload.as_bytes())?;
        }
        file.sync_all()?;
        // The hard-link operation refuses an existing destination atomically,
        // including symlinks. No check-then-rename race can overwrite an archive.
        parent.hard_link(&temporary, &parent, final_name)
    })();
    drop(file);
    // Publication has already committed. Temporary-name cleanup is best effort;
    // a cleanup error must not report a successfully published archive as lost.
    let _ = parent.remove_file(&temporary);
    result?;
    sync_directory(&parent)
}
