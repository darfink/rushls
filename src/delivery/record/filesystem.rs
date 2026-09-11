//! Directory-descriptor-relative writes keep publisher names inside the archive.
//! Linking the synced temporary inode into place provides an atomic no-replace
//! commit on both Linux and macOS; ordinary rename would overwrite old archives.
use crate::domain::Payload;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::Path,
};
use uuid::Uuid;

pub fn root(path: &Path) -> io::Result<File> {
    std::fs::create_dir_all(path)?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // create_dir_all may have introduced several root ancestors. Persist their
    // directory entries too, so a synced recording cannot lose its whole root
    // after a crash. Ancestors are operator-configured, not publisher input.
    for ancestor in path.ancestors().skip(1) {
        File::open(if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        })?
        .sync_all()?;
    }
    Ok(root)
}
fn name(value: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(io::Error::other)
}
fn checked_fd(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a successful openat returns a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn directory(parent: &File, name: &CString) -> io::Result<File> {
    // SAFETY: parent is held open and name is a terminated single component.
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o750) };
    if created < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: no symlink traversal; the returned descriptor owns the child.
    let child = checked_fd(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    if created == 0 {
        parent.sync_all()?;
    }
    Ok(child)
}
pub fn write(root: &File, path: &Path, payloads: &[Payload]) -> io::Result<()> {
    let mut parent = root.try_clone()?;
    let components: Vec<_> = path.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(io::Error::other("archive path is not relative"));
    }
    for component in &components[..components.len() - 1] {
        parent = directory(&parent, &name(component.as_os_str())?)?;
    }
    let final_name = name(components.last().expect("nonempty path").as_os_str())?;
    let temporary =
        CString::new(format!(".rushls-{}.tmp", Uuid::now_v7())).expect("UUID contains no NUL");
    // SAFETY: exclusive creation under our directory descriptor; never follows a symlink.
    let mut file = checked_fd(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o640,
        )
    })?;
    let result = (|| {
        for payload in payloads {
            file.write_all(payload.as_bytes())?;
        }
        file.sync_all()?;
        // SAFETY: both names are single components under the same open directory.
        // linkat refuses an existing destination atomically, including symlinks.
        if unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                final_name.as_ptr(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    // SAFETY: remove only the unique temporary name created by this invocation.
    //
    // Deliberately best-effort. `linkat` above is the commit point: once it
    // succeeds the archive is durable under `final_name`, and this call only
    // unlinks a name nothing can reach. Failing the whole recording because a
    // cleanup of an already-committed segment failed would report a good
    // archive as lost, and the leftover temporary is unreachable either way.
    // The directory sync below stays fatal: it is what makes the commit durable.
    let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
    result?;
    parent.sync_all()
}
