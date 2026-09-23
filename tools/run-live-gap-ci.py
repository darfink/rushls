#!/usr/bin/env python3
"""Run the live Rust origin and its Chrome playback probe as one failing check."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', required=True)
    parser.add_argument('--hls-js', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    subprocess.run(['cargo', 'test', '--locked', '--lib', '--no-run'], check=True)
    failures = []
    with (args.output / 'driver.log').open('w') as driver_log:
        driver = subprocess.Popen([args.driver, '--port=4446'], stdout=driver_log, stderr=subprocess.STDOUT)
        try:
            for _ in range(100):
                try:
                    with urllib.request.urlopen('http://127.0.0.1:4446/status', timeout=1):
                        break
                except OSError:
                    time.sleep(.1)
            else:
                raise RuntimeError('ChromeDriver did not start')
            for case in ('control', 'gaps'):
                ready = args.output / f'{case}.ready'
                for suffix in ('.ready', '.start', '.done', '.outcome', '.events', '.browser'):
                    ready.with_suffix(suffix).unlink(missing_ok=True)
                env = {**os.environ, 'RUSHLS_GAP_LIVE_READY': str(ready.resolve())}
                env.pop('RUSHLS_GAP_LIVE_CONTROL', None)
                if case == 'control':
                    env['RUSHLS_GAP_LIVE_CONTROL'] = '1'
                with (args.output / f'{case}.log').open('w') as log:
                    origin = subprocess.Popen(['cargo', 'test', '--locked', '--lib',
                        'live_av_gap_browser_origin', '--', '--ignored', '--nocapture'],
                        env=env, stdout=log, stderr=subprocess.STDOUT)
                    try:
                        probe_result = subprocess.run([sys.executable, 'tools/check-live-gap-playback.py',
                            '--ready', str(ready), '--hls-js', str(args.hls_js),
                            '--browser', 'chrome', '--switches', 'all',
                            '--output', str(args.output / f'{case}.json')], check=False, timeout=240)
                        if probe_result.returncode != 0:
                            failures.append(f'{case}: browser probe exited {probe_result.returncode}; see {case}.json')
                        if origin.wait(timeout=150) != 0:
                            raise RuntimeError(f'{case}: Rust origin failed; see {log.name}')
                    except (subprocess.SubprocessError, RuntimeError) as error:
                        failures.append(f'{case}: {error}')
                    finally:
                        ready.with_suffix('.done').touch()
                        origin.terminate()
                        origin.wait(timeout=15)
        finally:
            driver.terminate()
            driver.wait(timeout=15)
    if failures:
        raise RuntimeError('\n'.join(failures))


if __name__ == '__main__':
    main()
