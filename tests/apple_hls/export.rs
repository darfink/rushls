//! Save actual origin responses for the separate browser regression probe.
use std::{
    collections::{BTreeSet, VecDeque},
    path::Path,
    process::Command,
};
use url::Url;

pub fn save(
    origin: &str,
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let root = Url::parse(origin)?.join(".")?;
    let mut queue = VecDeque::from([Url::parse(origin)?]);
    let mut seen = BTreeSet::new();
    while let Some(url) = queue.pop_front() {
        if !seen.insert(url.to_string()) {
            continue;
        }
        let relative = url
            .path()
            .strip_prefix(root.path())
            .ok_or("resource outside fixture")?;
        let destination = directory.join(relative);
        let playlist_file = destination
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"));
        let response = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--max-time",
                "20",
                url.as_str(),
            ])
            .output()?;
        if !response.status.success() {
            return Err(format!("cannot export {url}").into());
        }
        let bytes = if playlist_file {
            let mut playlist = String::from_utf8(response.stdout)?;
            let mut gap = false;
            for line in playlist.clone().lines() {
                if line == "#EXT-X-GAP" {
                    gap = true;
                }
                let resource = if line.starts_with('#') {
                    line.split("URI=\"")
                        .nth(1)
                        .and_then(|s| s.split('"').next())
                } else if line.is_empty() {
                    None
                } else {
                    Some(line)
                };
                if let Some(resource) = resource {
                    let target = url.join(resource)?;
                    if !target.as_str().starts_with(root.as_str()) {
                        return Err("external fixture resource".into());
                    }
                    let local = url
                        .make_relative(&target)
                        .ok_or("cannot relativize fixture URI")?;
                    playlist = playlist.replace(resource, &local);
                    if !(gap || line.contains("GAP=YES")) {
                        queue.push_back(target);
                    }
                    if !line.starts_with('#') {
                        gap = false;
                    }
                }
            }
            playlist.into_bytes()
        } else {
            response.stdout
        };
        std::fs::create_dir_all(destination.parent().ok_or("missing parent")?)?;
        if playlist_file {
            let text = std::str::from_utf8(&bytes)?;
            let full = text
                .lines()
                .filter(|line| {
                    !line.starts_with("#EXT-X-PART")
                        && !line.starts_with("#EXT-X-PRELOAD-HINT")
                        && !line.starts_with("#EXT-X-SERVER-CONTROL")
                        && !line.starts_with("#EXT-X-RENDITION-REPORT")
                })
                .map(|line| line.replace(".m3u8", "-full.m3u8"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let name = destination
                .file_stem()
                .ok_or("playlist name")?
                .to_string_lossy();
            std::fs::write(
                destination.with_file_name(format!("{name}-full.m3u8")),
                full,
            )?;
        }
        std::fs::write(destination, bytes)?;
    }
    Ok(())
}
