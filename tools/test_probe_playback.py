import datetime
import http.server
import threading
import unittest
from probe_playback import latest_media, playlists, resolve, run


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        routes = {
            "/master.m3u8": b'#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nvideo.m3u8\n',
            "/video.m3u8": b'#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-09-14T12:00:00Z\n#EXTINF:1,\nsegment.m4s\n#EXT-X-PART:DURATION=0.5,URI="part.m4s"\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI="missing.m4s"\n',
            "/segment.m4s": b"segment",
            "/part.m4s": b"part",
        }
        body = routes.get(self.path)
        self.send_response(200 if body else 404)
        self.send_header("Content-Length", str(len(body or b"")))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def log_message(self, *_):
        pass


class PlaybackProbeTests(unittest.TestCase):
    def test_completed_parts_are_not_counted_twice(self):
        media, end, ended = latest_media("https://example.test/v.m3u8", '#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-09-14T12:00:00Z\n#EXT-X-PART:DURATION=1,URI="p1"\n#EXTINF:1,\ns1\n#EXT-X-PART:DURATION=0.5,URI="p2"\n#EXT-X-PRELOAD-HINT:URI="future"\n')
        self.assertEqual(media, "https://example.test/p2")
        self.assertEqual(end, datetime.datetime(2026, 9, 14, 12, 0, tzinfo=datetime.timezone.utc).timestamp() + 1.5)
        self.assertFalse(ended)

    def test_gap_does_not_advance_the_available_media_timestamp(self):
        media, end, _ = latest_media("https://example.test/v.m3u8", '#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-09-14T12:00:00Z\n#EXTINF:1,\ns1\n#EXT-X-GAP\n#EXTINF:10,\nmissing\n')
        self.assertEqual(media, "https://example.test/s1")
        self.assertEqual(end, datetime.datetime(2026, 9, 14, 12, 0, tzinfo=datetime.timezone.utc).timestamp() + 1)

    def test_variable_resolution_and_subtitle_exclusion(self):
        self.assertEqual(resolve("https://example.test/v.m3u8?token=a%2Bb", "p?token={$token}"), "https://example.test/p?token=a%2Bb")
        urls = playlists("https://example.test/m.m3u8", '#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,URI="sub.m3u8"\n#EXT-X-MEDIA:TYPE=AUDIO,URI="audio.m3u8"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo.m3u8\n')
        self.assertEqual(urls, ["https://example.test/audio.m3u8", "https://example.test/video.m3u8"])

    def test_actual_http_probe_fetches_published_media_not_preload_hint(self):
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                rows = run(f"http://127.0.0.1:{server.server_port}/master.m3u8", 2, None, 4, 1024)
                self.assertIn(("rushls_probe_media_bytes", 'rendition="0"', 4), rows)
            finally:
                server.shutdown()
                thread.join()


if __name__ == "__main__":
    unittest.main()
