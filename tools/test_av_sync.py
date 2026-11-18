"""Exercise signed A/V delays and the same verdict entrypoint used by CI."""
import array
import importlib.util
import itertools
import contextlib
import io
from pathlib import Path
import shutil
import statistics
import subprocess
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name('av-sync') / 'check-av-sync.py'
SPEC = importlib.util.spec_from_file_location('av_sync', SCRIPT)
av_sync = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(av_sync)


def detections(delay=0, *, jitter=False):
    times = [frame / 30 for frame in av_sync.marker_frames(14)]
    return {'flashes': times, 'beeps': [t + delay + (0.004 * (-1)**i if jitter else 0)
                                      for i, t in enumerate(times)],
            'errors': [], 'playing': True}


class AvSyncTests(unittest.TestCase):
    def test_signed_delays_do_not_wrap_at_whole_seconds(self):
        for delay in (0, 0.2, -0.2, 1, -1, 2, -2):
            with self.subTest(delay=delay):
                found = av_sync.offsets(detections(delay, jitter=True))
                self.assertGreaterEqual(len(found), 3)
                self.assertAlmostEqual(statistics.median(found), delay * 1000, delta=4.1)

    def test_startup_loss_and_missing_detections_preserve_identity(self):
        result = detections(2, jitter=True)
        result['flashes'] = result['flashes'][1:-1]
        result['beeps'] = result['beeps'][2:]
        del result['beeps'][2]
        found = av_sync.offsets(result)
        self.assertGreaterEqual(len(found), 3)
        self.assertAlmostEqual(statistics.median(found), 2000, delta=4.1)

    def test_two_missing_events_per_modality_do_not_alias(self):
        times = detections()['flashes']
        subsets = [list(s) for n in (7, 8, 9) for s in itertools.combinations(times, n)]
        for flashes, beeps in itertools.product(subsets, repeat=2):
            found = av_sync.offsets({'flashes': flashes, 'beeps': [t + 2 for t in beeps]})
            self.assertGreaterEqual(len(found), 3)
            self.assertAlmostEqual(statistics.median(found), 2000)

    def test_insufficient_ambiguous_and_invalid_data_are_rejected(self):
        for result in (
                {'flashes': [], 'beeps': []},
                {'flashes': [1, 2], 'beeps': [1, 2]},
                {'flashes': list(range(8)), 'beeps': list(range(2, 10))},
                {'flashes': [1, 2, 4, 11, 12, 14], 'beeps': [1, 2, 4]},
                {'flashes': [1, 2, 4], 'beeps': [1, float('nan'), 4]}):
            with self.subTest(result=result):
                self.assertEqual(av_sync.offsets(result), [])

    def test_browser_verdict_detects_whole_second_offsets(self):
        args = SimpleNamespace(players='hls.js', cases=['base', 'late'], listen=12, tolerance_ms=15)
        for delay in (0, 0.2, -0.2, 1, -1, 2, -2):
            with self.subTest(delay=delay), patch.object(av_sync, 'measure',
                    side_effect=[detections(), detections(delay)]), contextlib.redirect_stdout(io.StringIO()):
                report = {}
                failed = av_sync.run_browser('chrome', None, None, None, args, report)
                self.assertEqual(failed, delay != 0)
                self.assertAlmostEqual(report['chrome/hls.js']['late']['median_ms'], delay * 1000)

    def test_whole_second_error_common_to_every_case_fails_the_gate(self):
        args = SimpleNamespace(players='hls.js', cases=['base', 'late'], listen=12, tolerance_ms=15)
        for delay in (-1, 1):
            with self.subTest(delay=delay), patch.object(av_sync, 'measure', return_value=detections(delay)):
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertTrue(av_sync.run_browser('chrome', None, None, None, args, {}))

    @unittest.skipUnless(shutil.which('ffmpeg'), 'FFmpeg unavailable')
    def test_actual_publisher_signal_has_matching_irregular_markers(self):
        with tempfile.TemporaryDirectory() as temporary:
            media = Path(temporary) / 'signal.mkv'
            with patch.object(av_sync.subprocess, 'Popen') as popen:
                av_sync.publish('ffmpeg', 'rtmp://unused', 'base', 0, 0, 14)
            command = [arg for arg in popen.call_args.args[0] if arg != '-re']
            command[-2:] = ['matroska', str(media)]
            subprocess.run(command, check=True, capture_output=True, timeout=30)
            video = subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(media),
                                             '-vf', 'scale=1:1', '-pix_fmt', 'gray',
                                             '-f', 'rawvideo', '-an', 'pipe:1'], timeout=30)
            flashes = [i / 30 for i, pixel in enumerate(video) if pixel > 128]
            audio = array.array('f')
            audio.frombytes(subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(media),
                                                      '-vn', '-ac', '1', '-ar', '48000',
                                                      '-f', 'f32le', 'pipe:1'], timeout=30))
            beeps, quiet = [], 0
            for i, sample in enumerate(audio):
                if abs(sample) > 0.2:
                    if quiet > 24000:
                        beeps.append(i / 48000)
                    quiet = 0
                else:
                    quiet += 1
            self.assertEqual(len(flashes), len(list(av_sync.marker_frames(14))))
            self.assertEqual(len(beeps), len(flashes))
            self.assertLess(abs(statistics.median(av_sync.offsets({'flashes': flashes, 'beeps': beeps}))), 40)


if __name__ == '__main__':
    unittest.main()
