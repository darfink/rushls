# Releasing

User-facing platform, download, and container details are in [Installation](../installation.md).

Rushls uses the MIT license. Third-party dependencies retain their own licenses.
The hls.js source patch retains its upstream Apache 2.0 license.

## What CI builds

The CI workflow builds and tests native archives on every pull request and main-branch push:

| Archive suffix | Platform | Build environment |
| --- | --- | --- |
| `linux_amd64.tar.gz` | Linux x86-64 | Ubuntu 24.04, GNU libc |
| `linux_arm64.tar.gz` | Linux ARM64 | Ubuntu 24.04, GNU libc |
| `darwin_amd64.tar.gz` | macOS Intel | macOS 15 |
| `darwin_arm64.tar.gz` | macOS Apple Silicon | macOS 15 |
| `windows_amd64.zip` | Windows x86-64 | Windows Server 2025 |

Linux archives target Ubuntu 24.04 or compatible systems with glibc 2.39 or later.
They are not static musl binaries. Older Linux distributions are not validated release targets.
macOS archives are tested on macOS 15; older macOS versions are not covered by this matrix.

Each archive contains the executable, the annotated configuration example, README, documentation, MIT license, and dependency notices.
CI extracts the archive and runs the extracted executable on its native platform.
It checks version output, exact example-config output, HTTP readiness, and graceful shutdown.
The full media and player tests run separately in the same workflow.

Windows archives include `rushls.exe`. Windows library tests and archive smoke tests run on a native runner.
Use a local filesystem with hard-link support, such as NTFS, for recording.
Recording traverses directory handles without following symlinks or junctions and commits without overwriting an existing name.
Portable names exclude reserved Windows device names, alternate data streams, and trailing-dot/space aliases on every platform.

File contents are flushed before publication on all platforms.
Unix also flushes directory metadata. Windows does not claim equivalent directory-entry durability across sudden power loss.
Process termination is separate: Windows Ctrl+C and Ctrl+Break request graceful shutdown and drain pending work.

Disk retention uses held file locks for both root exclusivity and generation ownership.
Crash cleanup removes only marked, unlocked generations while holding the root lock.
Legacy `owner.pid` directories are left untouched; remove them manually while Rushls is stopped if necessary.
The Unix named-pipe backpressure test and Kubernetes projected-secret symlink-swap test remain Unix-only.
Windows runs the common storage suite, junction confinement, concurrent commit, and cross-process crash cleanup tests.

## Create a release

1. Set the root package version in `Cargo.toml` and update `Cargo.lock`.
2. Update release notes and known limitations. Use a prerelease version such as `0.1.0-rc.1` for a release candidate.
3. Merge the release commit into `main` and let CI finish.
4. Create and push the matching annotated tag, for example:

```sh
git tag -a v0.1.0-rc.1 -m 'Rushls v0.1.0-rc.1'
git push origin v0.1.0-rc.1
```

The workflow rejects tags that differ from the package version or point outside main's history.
Tag builds repeat the complete validation pipeline, including Apple packaging and the two-hour audit.
Binary builds and smoke tests also block publication.

The workflow publishes a versioned container, then creates a draft GitHub Release and uploads all five archives and `checksums.sha256`.
It publishes the draft only after successful upload. Prerelease versions become GitHub prereleases.
Published release assets are not overwritten on a rerun; an incomplete draft can be retried.
Tag creation is a separate operator action. Adding this workflow does not create a release.

## crates.io

After the GitHub Release is published, the **Publish to crates.io** job publishes the workspace.
`cargo publish --workspace` verifies every package first, then uploads the internal crates
(`rushls-config`, `rushls-metrics`, `rushls-outbound`, `rushls-tls`, `rushls-hooks`) before `rushls`.
The `rushls` binary depends on them, so they must share its release.
Bump an internal crate's version, and its `version` in `[workspace.dependencies]`, when its code changes.
An upload is permanent; it can be yanked but not replaced.

The first release has no crates to attach a trusted publisher to:

1. Create a crates.io API token with the `publish-new` and `publish-update` scopes, restricted to `rushls*`, with a short expiry.
2. Store it as the `CARGO_REGISTRY_TOKEN` secret of the `crates-io` environment.
3. After the release, add a trusted publisher to each of the six crates on crates.io:
   repository `darfink/rushls`, workflow `ci.yaml`, environment `crates-io`.
4. Delete the secret and revoke the token. Later releases authenticate through OIDC.

If the job fails after some uploads, rerun the remaining crates locally with
`cargo publish --workspace --exclude <published crate> ...` from the tagged commit.

Container tags follow this policy:

- `edge`: latest passing main-branch build.
- `sha-<commit>`: the exact passing commit.
- `v<version>`: the tagged release, including prereleases.
- `latest`: a stable tagged release; prereleases and main-branch builds do not update it.

The container currently targets Linux AMD64. Native ARM64 binaries are separate release assets.
