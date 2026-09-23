#!/usr/bin/env python3
"""Renew the LocalCert certificate and atomically publish its PEM pair to GitHub."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import time
import urllib.request

SERVER = 'https://acme-v02.api.letsencrypt.org/directory'
ACCOUNT_FILES = ('private_key.json', 'regr.json', 'meta.json')
RENEW_BEFORE = 30 * 24 * 60 * 60


def run(args, *, data=None):
    result = subprocess.run(args, input=data, capture_output=True, text=True)
    if result.returncode:
        # Neither command output nor exception bodies can leak PEMs or API responses.
        raise RuntimeError(f'{Path(args[0]).name} failed; credentials and output withheld')
    return result.stdout


def credentials():
    value = json.loads(os.environ['LOCALCERT_CREDENTIALS'])
    if not re.fullmatch(r'[a-z0-9-]+', value['subdomain']) or not value['password']:
        raise RuntimeError('Invalid LocalCert credentials')
    host = value['subdomain'] + '.localcert.net'
    if host != os.environ['APPLE_HLS_TLS_HOST']:
        raise RuntimeError('LocalCert credentials do not match the configured hostname')
    return value, host


def dns_hook():
    value, host = credentials()
    if os.environ.get('CERTBOT_IDENTIFIER', os.environ.get('CERTBOT_DOMAIN')) != host:
        raise RuntimeError('Refusing a DNS challenge for an unexpected hostname')
    validation = os.environ['CERTBOT_VALIDATION']
    request = urllib.request.Request(
        'https://api.localcert.net/v1/acme/create',
        data=json.dumps({**value, 'acme_token': validation}).encode(),
        headers={'Content-Type': 'application/json'}, method='POST')
    with urllib.request.urlopen(request, timeout=30) as response:
        if not json.load(response).get('success'):
            raise RuntimeError('LocalCert rejected the DNS challenge')
    for _ in range(60):
        visible = True
        for resolver in ('1.1.1.1', '8.8.8.8'):
            result = subprocess.run(
                ['dig', '+time=3', '+tries=1', '+short', '@' + resolver,
                 'TXT', '_acme-challenge.' + host], capture_output=True, text=True)
            # Match the TXT value, rather than a substring of a stale challenge.
            visible &= result.returncode == 0 and validation in [
                line.strip('"') for line in result.stdout.splitlines()]
        if visible:
            print('LocalCert DNS challenge is visible.')
            return
        time.sleep(5)
    raise RuntimeError('LocalCert DNS propagation timed out')


def write_private(path, text):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.write_text(text)
    path.chmod(0o600)


def validate_pair(directory, bundle, host):
    certificate, key = directory / 'cert.pem', directory / 'key.pem'
    write_private(certificate, bundle['certificate'])
    write_private(key, bundle['private_key'])
    match = run(['openssl', 'x509', '-in', str(certificate), '-noout', '-checkhost', host])
    if match.strip() != f'Hostname {host} does match certificate':
        raise RuntimeError('Certificate does not match the configured hostname')
    public = run(['openssl', 'x509', '-in', str(certificate), '-pubkey', '-noout'])
    if public != run(['openssl', 'pkey', '-in', str(key), '-pubout']):
        raise RuntimeError('Certificate and private key do not match')
    return certificate


def renewal_due(certificate):
    result = subprocess.run(['openssl', 'x509', '-in', str(certificate), '-noout',
                             '-checkend', str(RENEW_BEFORE)], capture_output=True)
    return result.returncode != 0


def restore_account(config, account):
    account_id = account['id']
    if not re.fullmatch(r'[a-f0-9]{32}', account_id):
        raise RuntimeError('Invalid Certbot account identifier')
    directory = config / 'accounts/acme-v02.api.letsencrypt.org/directory' / account_id
    for name in ACCOUNT_FILES:
        write_private(directory / name, json.dumps(account['files'][name]))
    return account_id


def renew():
    _, host = credentials()
    bundle = json.loads(os.environ['APPLE_HLS_TLS'])
    # Detect an expired automation token before certificate renewal becomes urgent.
    run(['gh', 'api', f"repos/{os.environ['GITHUB_REPOSITORY']}/actions/secrets/public-key", '--silent'])
    with tempfile.TemporaryDirectory(prefix='rushls-cert-') as scratch:
        root = Path(scratch)
        certificate = validate_pair(root / 'current', bundle, host)
        if not renewal_due(certificate):
            print('Certificate has more than 30 days remaining; no renewal needed.')
            return
        config = root / 'acme'
        account_id = restore_account(config, json.loads(os.environ['APPLE_HLS_ACME_ACCOUNT']))
        hook = shlex.join([sys.executable, str(Path(__file__).resolve()), '--dns-hook'])
        run(['certbot', 'certonly', '--non-interactive', '--manual',
             '--preferred-challenges', 'dns', '--manual-auth-hook', hook,
             '--server', SERVER, '--account', account_id,
             '--config-dir', str(config), '--work-dir', str(root / 'work'),
             '--logs-dir', str(root / 'logs'), '--cert-name', host,
             '--key-type', 'ecdsa', '-d', host])
        live = config / 'live' / host
        replacement = {'certificate': (live / 'fullchain.pem').read_text(),
                       'private_key': (live / 'privkey.pem').read_text()}
        certificate = validate_pair(root / 'new', replacement, host)
        if renewal_due(certificate):
            raise RuntimeError('Replacement certificate has insufficient remaining validity')
        # One encrypted GitHub secret keeps concurrent consumers on a matching pair.
        run(['gh', 'secret', 'set', 'APPLE_HLS_TLS', '--repo', os.environ['GITHUB_REPOSITORY']],
            data=json.dumps(replacement))
        print('Renewed certificate and key published together to APPLE_HLS_TLS.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dns-hook', action='store_true')
    args = parser.parse_args()
    os.umask(0o077)
    try:
        dns_hook() if args.dns_hook else renew()
    except Exception:
        # API responses, malformed secret values, and subprocess output stay private.
        print('Certificate renewal failed. Check credentials, token permissions, hostname, and DNS availability.', file=sys.stderr)
        sys.exit(1)
