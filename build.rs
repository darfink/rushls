use std::{fs, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/source/transport/srt/native.c");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    emit_git_sha();

    let library = pkg_config::Config::new()
        .atleast_version("1.5.5")
        .statik(true)
        .cargo_metadata(false)
        .probe("srt")
        .expect("libSRT 1.5.5 or newer must be discoverable through pkg-config");

    let mut shim = cc::Build::new();
    shim.file("src/source/transport/srt/native.c")
        .warnings(true);
    for include in &library.include_paths {
        shim.include(include);
    }
    shim.compile("rushls_srt_native");

    for path in &library.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for framework in &library.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    for path in &library.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }

    println!("cargo:rustc-link-lib=static=srt");
    for dependency in library.libs.iter().filter(|name| name.as_str() != "srt") {
        println!("cargo:rustc-link-lib=dylib={dependency}");
    }
}

/// Bake the commit this binary was built from into `GIT_SHA` for `env!`.
///
/// CI may set `GIT_SHA` when `.git` is absent; otherwise we ask git and mark
/// dirty trees so operators can tell a local build from a release commit.
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

    let sha = git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git_output(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    let value = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=GIT_SHA={value}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
