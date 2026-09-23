# Credential review

The September 23, 2026 review used Gitleaks 8.30.1 with its default rules and full redaction.
It scanned reachable local Git history across all refs and a snapshot of tracked files.
Reports remain in the ignored `target/security-review` directory; they are not release assets.

The history scan found two candidates, both from the same original example:

- A commented HTTP authorization URL in an older `rushls.toml`.
- The same URL in a configuration test, still present in the current tree.

Both point to a generic internal `auth-sidecar` endpoint.
Neither contains user credentials, a query token, or an API key.
They are false positives. No confirmed credential was found by this scan.

This scan does not establish that every possible secret is absent.
It does not inspect remote unreachable commits, GitHub secret values, private ignored runtime files, or external artifacts.
The LocalCert credentials and certificate key used for validation belong in GitHub secrets and ignored local files.
Do not copy them into release archives or tracked configuration.
