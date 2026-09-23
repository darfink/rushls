import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("apple_summary", Path(__file__).with_name("summarize-apple-hls.py"))
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class AppleSummaryTests(unittest.TestCase):
    def test_preserves_severities_nested_text_and_scopes(self):
        parsed = summary.Findings()
        parsed.feed('''<h2>General requirements</h2>
            <h3>HLS Spec Must Fix Issues</h3><h4>1. Missing <em>control</em> [#12]</h4>
            <ul><li>All I-Frame Variants</li></ul>
            <h3>Authoring Spec Should Fix Issues</h3><h4>2. Use TLS</h4>
            <ul><li>Media &amp; segments</li></ul>
            <h3>Report Information</h3><h4>Not a finding</h4>''')
        self.assertEqual(parsed.rows, [
            ["General requirements", "HLS Spec Must Fix Issues", "Missing control [#12]", ["All I-Frame Variants"]],
            ["General requirements", "Authoring Spec Should Fix Issues", "Use TLS", ["Media & segments"]],
        ])

    def test_missing_report_does_not_claim_success(self):
        with tempfile.TemporaryDirectory() as directory:
            text = summary.summarize(directory, "skipped")
        self.assertIn("**Test outcome:** skipped", text)
        self.assertIn("No two-hour HTML report was produced", text)

    def test_empty_or_unrecognised_report_does_not_claim_conformance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "two_hour_live_authoring.html").write_text("<h4>Unexpected new layout</h4>")
            text = summary.summarize(root, "failure")
        self.assertIn("does not establish conformance", text)
        self.assertIn("**Test outcome:** failure", text)

    def test_summary_escapes_html_and_markdown_table_content(self):
        text = summary.escape("<script>x</script>|[link](url)")
        self.assertNotIn("<script>", text)
        self.assertIn(r"\|", text)
        self.assertIn(r"\[link\]", text)


if __name__ == "__main__":
    unittest.main()
