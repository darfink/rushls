#!/usr/bin/env python3
"""Print `--exclude` arguments for workspace crates already on crates.io.

`cargo publish --workspace` refuses the whole run when any member's version
is already published, and a release rarely changes every crate. Excluding
the ones crates.io already has publishes the rest, still in dependency order.
Prints nothing when every crate is new, and `--exclude` for all of them when
none is, which the caller treats as nothing to do.
"""
import json
import subprocess
import urllib.error
import urllib.request

metadata = json.loads(subprocess.check_output(
    ['cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked']))
members = set(metadata['workspace_members'])
for package in metadata['packages']:
    if package['id'] not in members or package.get('publish') == []:
        continue
    url = f"https://crates.io/api/v1/crates/{package['name']}/{package['version']}"
    request = urllib.request.Request(url, headers={'User-Agent': 'rushls-release (github.com/darfink/rushls)'})
    try:
        with urllib.request.urlopen(request, timeout=30):
            print(f"--exclude {package['name']}")
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
