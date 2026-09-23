#!/usr/bin/env python3
"""Download the official driver matching the Chrome major version on a CI runner."""
import argparse
import io
import json
from pathlib import Path
import re
import subprocess
import urllib.request
import zipfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--chrome', default='google-chrome')
parser.add_argument('--platform', choices=('linux64', 'mac-arm64', 'mac-x64'), default='linux64')
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
version = subprocess.check_output([args.chrome, '--version'], text=True)
major = re.search(r'\d+', version).group()
with urllib.request.urlopen('https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json') as response:
    releases = json.load(response)
downloads = releases['milestones'][major]['downloads']['chromedriver']
url = next(item['url'] for item in downloads if item['platform'] == args.platform)
with urllib.request.urlopen(url) as response:
    archive = zipfile.ZipFile(io.BytesIO(response.read()))
    # Extract only the executable, not arbitrary archive paths.
    data = archive.read(f'chromedriver-{args.platform}/chromedriver')
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_bytes(data)
args.output.chmod(0o755)
print(f'Installed ChromeDriver for Chrome {major}: {args.output}')
