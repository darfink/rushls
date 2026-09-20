"""Checks for the load harness itself; no running origin required."""
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import soak


class HarnessTests(unittest.TestCase):
    def test_gap_is_not_downloaded_and_initialization_is_preserved(self):
        child, media = soak.playlist('''#EXTM3U
#EXT-X-MAP:URI="init/1.mp4"
#EXTINF:2,
segment/1.m4s
#EXT-X-GAP
#EXTINF:0.04,
segment/2.m4s
#EXTINF:2,
segment/3.m4s
''', "http://localhost/live/0/index.m3u8")
        self.assertEqual(child, [])
        self.assertEqual(media, ["http://localhost/live/0/init/1.mp4",
                                "http://localhost/live/0/segment/1.m4s",
                                "http://localhost/live/0/segment/3.m4s"])

    def test_master_includes_audio_and_video(self):
        children, media = soak.playlist('''#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,URI="audio/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1
video/index.m3u8
''', "http://localhost/live/index.m3u8")
        self.assertEqual(len(children), 2)
        self.assertEqual(media, [])

    def test_viewer_fetches_bytes_and_reports_errors(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(404 if self.path == "/missing" else 200)
                self.end_headers()
                self.wfile.write(b"media")

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            base = f"http://127.0.0.1:{server.server_port}"
            viewer = soak.Viewer(base, threading.Event())
            self.assertEqual(viewer.get(base + "/segment/1", media=True), b"media")
            self.assertEqual(viewer.snapshot()["media"], 1)
            self.assertEqual(viewer.snapshot()["bytes"], 5)
            with self.assertRaises(OSError):
                viewer.get(base + "/missing")
        finally:
            server.shutdown()
            thread.join()
            server.server_close()

    def test_metrics_does_not_confuse_labelled_series_with_totals(self):
        self.assertEqual(soak.metrics('# HELP test\ncount 3\ncount{a="b"} 4\n'), {"count": 3})


if __name__ == "__main__":
    unittest.main()
