"""Regression checks for documentation extraction and meaningful media evidence."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('doc_examples', Path(__file__).with_name('check-doc-examples.py'))
examples = importlib.util.module_from_spec(spec)
spec.loader.exec_module(examples)


class DocumentationExamplesTests(unittest.TestCase):
    def test_extraction_preserves_the_documented_command(self):
        source = '<!-- verify: {"id":"demo","stream":"demo","video":1,"audio":1} -->\n```sh\nprintf "literal $HOME\\n"\n```\n'
        result = examples.extract_examples([('README.md', source)])
        self.assertEqual(result['demo']['command'], 'printf "literal $HOME\\n"\n')
        self.assertEqual(result['demo']['source'], 'README.md')
        self.assertEqual(result['demo']['line'], 1)

    def test_malformed_or_duplicate_markers_fail(self):
        marker = '<!-- verify: {"id":"fixture"} -->\n```sh\necho test\n```\n'
        for source in (marker + marker, marker.replace('```sh', '```text'), marker[:-4]):
            with self.subTest(source=source), self.assertRaises(ValueError):
                examples.extract_examples([('README.md', source)])

    def test_missing_tracks_and_wrong_languages_fail(self):
        master = ('#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE="en",URI="audio.m3u8"\n'
                  '#EXT-X-STREAM-INF:BANDWIDTH=1000,CODECS="avc1.64001e,mp4a.40.2"\nvideo.m3u8\n')
        expected = dict(video=1, audio=1, languages=['en'])
        renditions, counts = examples.verify_master(expected, master, 'http://localhost/live/test/index.m3u8')
        self.assertEqual(counts, dict(video=1, audio=1, subtitles=0))
        self.assertEqual(renditions[0][1], 'http://localhost/live/test/video.m3u8')
        for changes in (dict(video=2), dict(subtitles=1), dict(languages=['es'])):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                examples.verify_master(dict(expected, **changes), master, 'http://localhost/')

    def test_decoder_exit_without_media_is_not_success(self):
        self.assertFalse(examples.decoded('progress=end\n', 'audio'))
        self.assertFalse(examples.decoded('out_time_us=0\nframe=0\n', 'video'))
        self.assertFalse(examples.decoded('out_time_us=2000000\nframe=0\n', 'video'))
        self.assertTrue(examples.decoded('out_time_us=2000000\nframe=60\n', 'video'))
        self.assertTrue(examples.decoded('out_time_us=2000000\n', 'audio'))

    def test_subtitles_need_cue_text_not_only_a_priming_track(self):
        cue = '00:00:00.000 --> 00:00:02.000\nHello\n'
        self.assertTrue(examples.has_caption('WEBVTT\n', [cue], 'Hello'))
        self.assertFalse(examples.has_caption('WEBVTT\n', [''], 'Hello'))
        self.assertFalse(examples.has_caption('', [cue], 'Hello'))
        self.assertFalse(examples.has_caption('WEBVTT\n', [cue], 'Missing'))

    def test_required_repository_examples_are_present(self):
        root = Path(__file__).resolve().parent.parent
        found = examples.extract_examples([(name, (root / name).read_text()) for name in examples.DOCUMENTS])
        self.assertTrue(examples.REQUIRED <= found.keys())


if __name__ == '__main__':
    unittest.main()
