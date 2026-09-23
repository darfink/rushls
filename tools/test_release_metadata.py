"""Publication must reject mismatched tags and commits outside main's history."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('release-metadata.py').resolve()


class ReleaseMetadataTests(unittest.TestCase):
    def test_release_gates_and_tag_policy(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def git(*args):
                return subprocess.check_output(['git', *args], cwd=root, stderr=subprocess.DEVNULL, text=True).strip()

            git('init', '-b', 'main')
            git('config', 'user.name', 'Release Test')
            git('config', 'user.email', 'release@example.invalid')
            git('commit', '--allow-empty', '-m', 'main')
            git('update-ref', 'refs/remotes/origin/main', 'HEAD')

            def exercise(version, ref):
                (root / 'Cargo.toml').write_text(f'[package]\nversion = "{version}"\n')
                output = root / 'output'
                output.write_text('')
                result = subprocess.run([sys.executable, str(SCRIPT)], cwd=root, capture_output=True,
                    env={**os.environ, 'GITHUB_REF': ref, 'GITHUB_SHA': git('rev-parse', 'HEAD'),
                         'GITHUB_REPOSITORY': 'example/rushls', 'GITHUB_OUTPUT': str(output)})
                return result.returncode, output.read_text()

            code, output = exercise('0.1.0', 'refs/tags/v0.1.0')
            self.assertEqual(code, 0)
            self.assertIn('ghcr.io/example/rushls:latest\n', output)
            code, output = exercise('0.1.0-rc.1', 'refs/tags/v0.1.0-rc.1')
            self.assertEqual(code, 0)
            self.assertIn('prerelease=true', output)
            self.assertNotIn(':latest', output)
            code, output = exercise('0.1.0', 'refs/heads/main')
            self.assertEqual(code, 0)
            self.assertIn(':edge\n', output)
            self.assertNotIn(':latest', output)
            self.assertNotEqual(exercise('0.1.0', 'refs/tags/v0.2.0')[0], 0)
            git('commit', '--allow-empty', '-m', 'unmerged')
            self.assertNotEqual(exercise('0.1.0', 'refs/tags/v0.1.0')[0], 0)


if __name__ == '__main__':
    unittest.main()
