#!/usr/bin/env python3
"""Package and smoke-test the exact native binary distributed to users."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.request
import zipfile


def port(kind=socket.SOCK_STREAM):
    with socket.socket(socket.AF_INET, kind) as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def smoke(root, version):
    if os.name == 'nt':
        # Headless Windows runners may have no console. The child needs a
        # shared console for a targeted CTRL_BREAK_EVENT, not TerminateProcess.
        import ctypes
        kernel = ctypes.windll.kernel32
        if kernel.GetConsoleCP() == 0 and not kernel.AllocConsole():
            raise ctypes.WinError()
    binary = root / ('rushls.exe' if os.name == 'nt' else 'rushls')
    env = {k: v for k, v in os.environ.items() if not k.startswith('RUSHLS_')}
    actual = subprocess.check_output([str(binary), '--version'], env=env, text=True)
    if f' {version} (' not in actual:
        raise RuntimeError(f'Unexpected binary version: {actual}')
    example = subprocess.check_output([str(binary), '--print-config-example'], env=env)
    if example != (root / 'rushls.example.toml').read_bytes():
        raise RuntimeError('Embedded configuration differs from packaged example')
    with tempfile.TemporaryDirectory() as temporary:
        config = Path(temporary) / 'smoke.toml'
        http = port()
        config.write_text(f'[http]\nlisten = "127.0.0.1:{http}"\n'
                          f'[ingest.rtmp]\nlisten = "127.0.0.1:{port()}"\n'
                          f'[ingest.srt]\nlisten = "127.0.0.1:{port(socket.SOCK_DGRAM)}"\n')
        with (root / 'smoke.log').open('w') as log:
            process = subprocess.Popen([str(binary), '--config', str(config)], cwd=temporary,
                                       env=env, stdout=log, stderr=subprocess.STDOUT,
                                       creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
            try:
                deadline = time.monotonic() + 20
                while True:
                    if process.poll() is not None:
                        raise RuntimeError('Packaged binary exited during startup')
                    try:
                        with urllib.request.urlopen(f'http://127.0.0.1:{http}/health/ready', timeout=1) as response:
                            if response.status == 200:
                                break
                    except OSError:
                        pass
                    if time.monotonic() >= deadline:
                        raise RuntimeError('Packaged binary did not become ready')
                    time.sleep(.1)
            finally:
                if os.name == "nt":
                    process.send_signal(signal.CTRL_BREAK_EVENT)
                else:
                    process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise RuntimeError('Packaged binary did not shut down gracefully')
            if process.returncode != 0:
                raise RuntimeError(f'Packaged binary shutdown failed: {process.returncode}')


def notices(destination):
    # Include resolved dependency notices verbatim; the project's MIT license
    # does not replace licenses supplied with dependencies.
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version=1']))
    with destination.open('w', encoding='utf-8') as output:
        output.write('# Dependency licenses and notices\n\n')
        for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
            if package['source'] is None:
                continue
            output.write(f"## {package['name']} {package['version']}\n\nDeclared license: {package['license']}\n\n")
            directory = Path(package['manifest_path']).parent
            candidates = [p for p in directory.iterdir()
                          if p.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'copyright'))]
            if package.get('license_file'):
                candidates.append(directory / package['license_file'])
            files = set()
            for candidate in candidates:
                files.update(candidate.rglob('*') if candidate.is_dir() else [candidate])
            for path in sorted(files):
                if path.is_file():
                    output.write(f'### {path.relative_to(directory)}\n\n')
                    output.write(path.read_text(encoding='utf-8', errors='replace') + '\n\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', required=True)
    parser.add_argument('--platform', required=True)
    parser.add_argument('--output', type=Path, default=Path('target/release-assets'))
    args = parser.parse_args()
    version = tomllib.loads(Path('Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    name = f'rushls_v{version}_{args.platform}'
    with tempfile.TemporaryDirectory() as temporary:
        stage = Path(temporary) / name
        stage.mkdir()
        binary = 'rushls.exe' if os.name == 'nt' else 'rushls'
        shutil.copy2(Path('target') / args.target / 'release' / binary, stage / binary)
        for filename in ('README.md', 'LICENSE', 'rushls.toml', 'rushls.example.toml'):
            shutil.copyfile(filename, stage / filename)
        shutil.copytree('docs', stage / 'docs')
        shutil.copytree('tools/patches/hls.js', stage / 'tools/patches/hls.js')
        shutil.copyfile('tools/build-hls-player.py', stage / 'tools/build-hls-player.py')
        notices(stage / 'THIRD-PARTY-NOTICES.md')
        unpacked = Path(temporary) / 'unpacked'
        if os.name == 'nt':
            archive = output / f'{name}.zip'
            with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as zipped:
                for path in sorted(stage.rglob('*')):
                    if path.is_file():
                        zipped.write(path, path.relative_to(stage.parent))
            with zipfile.ZipFile(archive) as zipped:
                zipped.extractall(unpacked)
        else:
            archive = output / f'{name}.tar.gz'
            with tarfile.open(archive, 'w:gz') as tar:
                tar.add(stage, arcname=name)
            with tarfile.open(archive) as tar:
                tar.extractall(unpacked, filter='data')
        smoke(unpacked / name, version)
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        (output / f'{name}.sha256').write_text(f'{checksum}  {archive.name}\n')
        print(f'Packaged and smoke-tested {archive.name}')


if __name__ == '__main__':
    main()
