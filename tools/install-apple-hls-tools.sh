#!/usr/bin/env bash
# Install an operator-provided Apple package on an ephemeral macOS CI runner.
set -euo pipefail
if test -z "${APPLE_HLS_TOOLS_URL:-}" && command -v mediastreamvalidator >/dev/null && command -v hlsreport >/dev/null; then
  mediastreamvalidator --version
  hlsreport --version
  exit 0
fi
: "${APPLE_HLS_TOOLS_URL:?Set APPLE_HLS_TOOLS_URL to a CI-accessible Apple HLS tools pkg or dmg}"
: "${APPLE_HLS_TOOLS_SHA256:?Set APPLE_HLS_TOOLS_SHA256 to the installer SHA-256}"
umask 077
installer_dir="$(mktemp -d)"
trap 'hdiutil detach "$installer_dir/mount" >/dev/null 2>&1 || true; rm -rf "$installer_dir"' EXIT
# Apple's installer must not be public, so it is a release asset of a private
# repository. Use the job's read-only token for GitHub assets; never send that
# token to an arbitrary installer host.
if [[ "$APPLE_HLS_TOOLS_URL" =~ ^https://github.com/([^/]+/[^/]+)/releases/download/([^/]+)/([^/]+)$ ]] && test -n "${GH_TOKEN:-}"; then
  gh release download "${BASH_REMATCH[2]}" --repo "${BASH_REMATCH[1]}" \
    --pattern "${BASH_REMATCH[3]}" --output "$installer_dir/download"
else
  curl --fail --silent --show-error --location "$APPLE_HLS_TOOLS_URL" -o "$installer_dir/download"
fi
if test -n "${APPLE_HLS_TOOLS_PASSPHRASE:-}"; then
  # Keep the passphrase off the command line and GnuPG state out of caches.
  mkdir "$installer_dir/gnupg"
  printf '%s' "$APPLE_HLS_TOOLS_PASSPHRASE" | gpg \
    --homedir "$installer_dir/gnupg" --batch --pinentry-mode loopback \
    --no-symkey-cache --passphrase-fd 0 \
    --output "$installer_dir/decrypted" --decrypt "$installer_dir/download"
  unset APPLE_HLS_TOOLS_PASSPHRASE
  mv "$installer_dir/decrypted" "$installer_dir/download"
fi
printf '%s  %s\n' "$APPLE_HLS_TOOLS_SHA256" "$installer_dir/download" | shasum -a 256 --check
if hdiutil imageinfo "$installer_dir/download" >/dev/null 2>&1; then
  hdiutil attach "$installer_dir/download" -nobrowse -readonly -mountpoint "$installer_dir/mount"
  package="$(find "$installer_dir/mount" -name '*.pkg' -maxdepth 3 -print -quit)"
  test -n "$package"
else
  package="$installer_dir/download"
fi
sudo installer -pkg "$package" -target /
mediastreamvalidator --version
hlsreport --version
