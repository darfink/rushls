# Apple HLS certificate renewal

The `Renew Apple HLS certificate` workflow checks the certificate daily at 05:17 UTC.
It runs on Ubuntu and permits manual runs from the default branch.
It never runs for pull requests. Concurrent renewal jobs wait for the current job to finish.

The job renews when the certificate has 30 days or less remaining.
It uses Certbot, the existing Let's Encrypt account, and a LocalCert DNS-01 hook.
The hook accepts only the configured hostname and waits for two DNS resolvers to confirm the challenge.
No inbound ports, macOS runner, or Keychain changes are needed.

## Repository configuration

| Setting | Type | Contents |
| --- | --- | --- |
| `CERT_RENEWAL_GH_TOKEN` | Secret | Fine-grained token for this repository with Secrets read/write permission |
| `LOCALCERT_CREDENTIALS` | Secret | JSON with `subdomain` and `password` from LocalCert |
| `APPLE_HLS_ACME_ACCOUNT` | Secret | JSON with Certbot account `id` and `files` containing `private_key.json`, `regr.json`, and `meta.json` objects |
| `APPLE_HLS_TLS` | Secret | JSON with PEM strings in `certificate` and `private_key` |
| `APPLE_HLS_TLS_HOST` | Variable | Certificate hostname, such as `example.localcert.net` |

The account comes from Certbot's `accounts/acme-v02.api.letsencrypt.org/directory/<id>/` directory.
Account files and the LocalCert password must remain secret.
The audit receives only the certificate bundle, not the DNS or account credentials.

The job checks token access on every run, even when renewal is not due.
After issuance, it checks the hostname, key match, and remaining validity.
It then updates `APPLE_HLS_TLS` in one API request, so the audit cannot read a mismatched certificate/key pair.
Failed issuance or validation leaves the existing secret unchanged.
Temporary files use private permissions and are removed on exit. No certificate artifacts or secret-bearing logs are uploaded.

## Operation

Run `Renew Apple HLS certificate` from the Actions page to check configuration immediately.
A healthy certificate produces `Certificate has more than 30 days remaining; no renewal needed.`
An expired token or failed renewal fails the job; use GitHub Actions failure notifications to detect it.
Replace the token secret before its chosen expiration date.
GitHub can disable scheduled workflows in public repositories after 60 days without repository activity; re-enable the workflow if that occurs.

The renewal helper requires Python 3, OpenSSL 3, Certbot, `dig`, and `gh`.
The Ubuntu workflow installs Certbot and DNS utilities; the runner supplies the remaining tools.
Run its network-free regression tests with:

```sh
python3 -m unittest discover -s tools -p test_cert_renewal.py
```

On macOS, put Homebrew OpenSSL 3 on `PATH` first; the system LibreSSL lacks the hostname-check option.
