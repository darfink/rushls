# Installation

## Release archives

Each release has an archive per platform:

| Archive suffix | Platform | Tested on |
| --- | --- | --- |
| `linux_amd64.tar.gz` | Linux x86-64 | Ubuntu 24.04 |
| `linux_arm64.tar.gz` | Linux ARM64 | Ubuntu 24.04 |
| `darwin_amd64.tar.gz` | macOS Intel | macOS 15 |
| `darwin_arm64.tar.gz` | macOS Apple Silicon | macOS 15 |
| `windows_amd64.zip` | Windows x86-64 | Windows Server 2025 |

Linux binaries need glibc 2.39 or later (Ubuntu 24.04 or a compatible system). They are not static musl builds.
Older Linux distributions and macOS versions are not tested.

An archive contains the `rushls` executable, the annotated example configuration, the README, this documentation,
the MIT license, and third-party notices.

Verify downloads against the release's checksum file before extracting them:

```sh
sha256sum --check --ignore-missing checksums.sha256        # Linux
shasum -a 256 --check --ignore-missing checksums.sha256    # macOS
```

Archives are not signed or notarized yet. A checksum detects a corrupted download; it is not a signature.

## Container image

```sh
docker run --rm -p 1935:1935 -p 9000:9000/udp -p 8080:8080 ghcr.io/darfink/rushls:latest
```

| Tag | Contents |
| --- | --- |
| `latest` | The latest stable release |
| `v<version>` | A tagged release, including prereleases |
| `edge` | The latest passing build of `main` |
| `sha-<commit>` | The build of one exact commit |

The image is built for Linux AMD64 and ARM64, and runs as UID/GID `65532:65532`.
It is based on distroless `cc` with a BusyBox shell, so `docker exec -it <container> sh` works for debugging; there is no package manager.
It ships [a configuration](../examples/container/rushls.toml) that listens on all container interfaces and lets anyone publish;
mount your own at `/etc/rushls/rushls.toml` for anything public. See [Deployment](deployment.md) for volumes and secrets.
`/var/lib/rushls` is writable by that user and is the working directory; point `disk.dir` and `record.dir` below it and mount a volume there.
Without `disk.dir`, spilled DVR media goes under `/var/lib/rushls/cache`.

## Windows

- Write paths in TOML with forward slashes, or as literal single-quoted strings: `dir = 'C:\Rushls\recordings'`.
- Recording and disk storage need a local filesystem with hard links, such as NTFS.
- Ctrl+C and Ctrl+Break shut down gracefully, like SIGTERM on Unix.
- File contents are flushed before a recording is published, but Windows does not guarantee that directory entries survive a sudden power loss the way Unix does.
