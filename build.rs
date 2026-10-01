use std::{fs, process::Command};

fn main() {
    emit_git_sha();
}

/// Bake the commit this binary was built from into `GIT_SHA` for `env!`.
///
/// CI may set `GIT_SHA` when `.git` is absent; otherwise we ask git and mark
/// dirty trees so operators can tell a local build from a release commit.
/// A crates.io source has no `.git`, but `cargo package` records the commit in
/// `.cargo_vcs_info.json`; asking git there could instead report whatever
/// repository happens to enclose the Cargo registry.
fn emit_git_sha() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    // Symbolic HEAD only changes when the branch moves; track the ref file too.
    if let Ok(head) = fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", reference.trim());
    }

    if let Ok(sha) = std::env::var("GIT_SHA")
        && !sha.is_empty()
    {
        println!("cargo:rustc-env=GIT_SHA={sha}");
        return;
    }

    if let Ok(info) = fs::read_to_string(".cargo_vcs_info.json") {
        let sha = packaged_sha(&info).unwrap_or("unknown");
        println!("cargo:rustc-env=GIT_SHA={sha}");
        return;
    }

    let sha = git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git_output(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    let value = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=GIT_SHA={value}");
}

/// The 12-character prefix of `git.sha1` in Cargo's packaging record.
///
/// A string scan rather than a JSON parser: the file has a fixed shape written
/// by Cargo, and a build dependency would be compiled for every install.
fn packaged_sha(info: &str) -> Option<&str> {
    let (_, rest) = info.split_once("\"sha1\"")?;
    let (_, rest) = rest.split_once('"')?;
    let (sha, _) = rest.split_once('"')?;
    sha.get(..12)
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
