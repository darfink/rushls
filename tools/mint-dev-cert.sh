#!/bin/sh
# Mint the short-lived certificate a browser pins with 'serverCertificateHashes'.
#
# Chromium accepts a pinned certificate only when it is ECDSA P-256 and its
# total validity is at most fourteen days. This is therefore not a certificate
# any CA would issue, and not one that can be renewed -- it is reminted, and
# its fingerprint changes every time. That is why the client reads
# /certificate.sha256 on every connection instead of caching a value.
#
# Thirteen days rather than fourteen leaves a day of slack, so a laptop asleep
# over a weekend, or a cron run that failed quietly, still finds a usable
# certificate rather than an opaque handshake failure.
#
# Run daily:
#   0 7 * * *  /path/to/rushls/tools/mint-dev-cert.sh
#
# rushls picks it up without a restart: rushls_common::tls watches the containing directory
# and swaps an ArcSwap, so live sessions keep the key they negotiated with.
set -eu

cert_dir="${1:-${RUSHLS_DEV_TLS_DIR:-$HOME/.rushls/dev-tls}}"
valid_days=13

mkdir -p "$cert_dir"

# Two steps rather than 'req -newkey ec', which spells the curve differently
# across OpenSSL and LibreSSL. Written beside the live pair and renamed into
# place, so a reload never reads a half-written file: a reload that catches a
# new certificate against the old key fails to build a key pair and is
# skipped, and the next filesystem event settles it.
openssl ecparam -name prime256v1 -genkey -noout \
  -out "$cert_dir/key.pem.pending"

openssl req -new -x509 \
  -key "$cert_dir/key.pem.pending" \
  -out "$cert_dir/cert.pem.pending" \
  -days "$valid_days" \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth" \
  >/dev/null 2>&1

chmod 600 "$cert_dir/key.pem.pending"
mv "$cert_dir/key.pem.pending" "$cert_dir/key.pem"
mv "$cert_dir/cert.pem.pending" "$cert_dir/cert.pem"

fingerprint=$(openssl x509 -in "$cert_dir/cert.pem" -outform der \
  | openssl dgst -sha256 -hex \
  | awk '{ print $NF }')
expires=$(openssl x509 -in "$cert_dir/cert.pem" -noout -enddate | cut -d= -f2)

printf 'certificate %s\n' "$cert_dir/cert.pem"
printf 'key         %s\n' "$cert_dir/key.pem"
printf 'expires     %s\n' "$expires"
printf 'sha-256     %s\n' "$fingerprint"
