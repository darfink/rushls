# Validation artifacts

Keep reusable test tools, small regression fixtures, and readable findings in Git.
Store generated captures under `target/rushls-validation/`, relative to the repository root.
The existing `target/` ignore rule excludes this directory from commits.

Local captures include browser reports, logs, generated media, packet traces, experimental patches, and snapshots of tools used during a run.
They are not available in a fresh clone. Paths in investigation notes identify local evidence, not downloadable GitHub resources.
Use the [GAP regression checks](regressions.md) to generate fresh evidence.

## Run directories

Create a separate directory for each investigation or run:

```sh
mkdir -p target/rushls-validation/2026-09-22-example
```

Use that directory for the probe's `--output` file and the origin's ready-file path.
Use fresh ready-file names: the origin also creates companion start, completion, and event files.
Record the source revision, player version and hash, browser version, commands, and result in the run directory.
Separate incomplete or superseded runs from accepted evidence.

The captures moved from `docs/validation/` retain their original directory names.
Their contents were verified with SHA-256 before and after the move.
`target/rushls-validation/capture-checksums.json` records those file hashes.
Archived scripts and reports preserve their original paths as historical evidence.

## Sharing and retention

For shared validation, upload the run directory as a CI artifact or to an artifact store.
Record the artifact URL, expiry date, and checksum beside the committed findings.
No upload or CI retention policy is configured by this local storage change.
A 30–90 day retention period is a reasonable default for routine runs.
Keep release evidence and upstream bug reproductions for as long as they remain useful.

The `target/` directory is disposable. Commands such as `cargo clean` can remove these captures.
Before cleaning it, copy evidence that must be retained to durable storage.
Do not force-add generated captures to Git to preserve them.
