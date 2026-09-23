#!/usr/bin/env python3
"""Validate version tags before any publication and select container tags."""
import os
from pathlib import Path
import re
import subprocess
import tomllib

version = tomllib.loads(Path('Cargo.toml').read_text())['package']['version']
ref = os.environ['GITHUB_REF']
sha = os.environ['GITHUB_SHA']
image = 'ghcr.io/' + os.environ['GITHUB_REPOSITORY'].lower()
tags = [f'{image}:sha-{sha}']
prerelease = '-' in version
if ref.startswith('refs/tags/'):
    tag = ref.removeprefix('refs/tags/')
    if tag != f'v{version}' or not re.fullmatch(r'v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?', tag):
        raise SystemExit('Release tag must equal v<Cargo.toml package version>')
    # Only reviewed main-branch commits may become public release artifacts.
    subprocess.run(['git', 'merge-base', '--is-ancestor', 'HEAD', 'origin/main'], check=True)
    tags.append(f'{image}:{tag}')
    if not prerelease:
        tags.append(f'{image}:latest')
else:
    tags.append(f'{image}:edge')
with Path(os.environ['GITHUB_OUTPUT']).open('a') as output:
    output.write(f'version={version}\nprerelease={str(prerelease).lower()}\n')
    output.write('image_tags<<IMAGE_TAGS\n' + '\n'.join(tags) + '\nIMAGE_TAGS\n')
