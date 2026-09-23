#!/usr/bin/env python3
"""Build the CI player from pinned upstream source and the reviewed local patch."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request

SOURCE = 'https://github.com/video-dev/hls.js.git'
COMMIT = 'e5ff3583965e3af16c4a4b4d2b5f7bd1ffb5b7de'
VERSION = '1.7.3-rushls.1'
OFFICIAL_URL = 'https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js'
OFFICIAL_SHA256 = 'a12e7ee1cd64a69dcdb314157e45dafcba705bfb0b1440b7935cb265d374423e'
PATCH = Path(__file__).resolve().parent / 'patches/hls.js/pending-parts.patch'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--unit-tests', action='store_true', help='Run upstream ChromeHeadless unit tests')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    official = output / 'hls.official.min.js'
    with urllib.request.urlopen(OFFICIAL_URL, timeout=60) as response:
        official.write_bytes(response.read())
    if digest(official) != OFFICIAL_SHA256:
        raise RuntimeError('Official player checksum mismatch')
    with tempfile.TemporaryDirectory(prefix='rushls-hls-') as temporary:
        source = Path(temporary)

        def run(*command):
            subprocess.run(command, cwd=source, check=True)

        run('git', 'init', '--quiet')
        run('git', 'fetch', '--quiet', '--depth=1', SOURCE, COMMIT)
        run('git', 'checkout', '--quiet', '--detach', 'FETCH_HEAD')
        actual = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
        if actual != COMMIT:
            raise RuntimeError('Unexpected upstream source revision')
        lock_hash = digest(source / 'package-lock.json')
        run('git', 'apply', '--check', str(PATCH))
        run('git', 'apply', str(PATCH))
        run('npm', 'ci', '--no-audit', '--no-fund')
        # Upstream injects the version during release; a source checkout has none.
        package = source / 'package.json'
        metadata = json.loads(package.read_text())
        metadata['version'] = VERSION
        package.write_text(json.dumps(metadata, indent=2) + '\n')
        run('node', 'node_modules/rollup/dist/bin/rollup', '--config', '--configType', 'full')
        shutil.copyfile(source / 'dist/hls.js', output / 'hls.patched.js')
        shutil.copyfile(source / 'LICENSE', output / 'LICENSE.hls.js')
        shutil.copyfile(PATCH, output / PATCH.name)
        manifest = {
            'source': SOURCE, 'commit': COMMIT, 'version': VERSION,
            'package_lock_sha256': lock_hash, 'patch_sha256': digest(PATCH),
            'bundle_sha256': digest(output / 'hls.patched.js'),
            'official_url': OFFICIAL_URL, 'official_sha256': OFFICIAL_SHA256,
            'node': subprocess.check_output(['node', '--version'], text=True).strip(),
            'npm': subprocess.check_output(['npm', '--version'], text=True).strip(),
            'unit_tests': 'pending' if args.unit_tests else 'not run',
        }
        provenance = output / 'provenance.json'
        provenance.write_text(json.dumps(manifest, indent=2) + '\n')
        if args.unit_tests:
            # Persist the log even when a test fails; CI must not hide player regressions.
            with (output / 'unit-tests.log').open('w') as log:
                result = subprocess.run(['node', 'node_modules/karma/bin/karma', 'start',
                    'karma.conf.js', '--single-run', '--browsers', 'ChromeHeadless'],
                    cwd=source, env={**os.environ, 'CI': 'true'}, stdout=log, stderr=subprocess.STDOUT)
            manifest['unit_tests'] = 'passed' if result.returncode == 0 else 'failed'
            provenance.write_text(json.dumps(manifest, indent=2) + '\n')
            result.check_returncode()


if __name__ == '__main__':
    main()
