"""Exercise expiry decisions and atomic secret updates without network requests."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('cert_renewal', Path(__file__).with_name('renew-apple-hls-cert.py'))
renewal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(renewal)
HOST = 'fixture.localcert.net'


class RenewalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory()
        cls.root = Path(cls.scratch.name)
        cls.fresh = cls.pair('fresh', 90)
        cls.due = cls.pair('due', 1)

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    @classmethod
    def pair(cls, name, days):
        cert, key = cls.root / (name + '.pem'), cls.root / (name + '.key')
        renewal.run(['openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt',
                     'ec_paramgen_curve:P-256', '-nodes', '-days', str(days),
                     '-subj', '/CN=' + HOST, '-addext', 'subjectAltName=DNS:' + HOST,
                     '-keyout', str(key), '-out', str(cert)])
        return {'certificate': cert.read_text(), 'private_key': key.read_text()}

    def exercise(self, current, replacement=None, fail_upload=False):
        calls = []
        real_run = renewal.run

        def run(args, *, data=None):
            if args[0] == 'certbot':
                calls.append(('issue', None))
                directory = Path(args[args.index('--config-dir') + 1]) / 'live' / HOST
                directory.mkdir(parents=True)
                (directory / 'fullchain.pem').write_text(replacement['certificate'])
                (directory / 'privkey.pem').write_text(replacement['private_key'])
                return ''
            if args[:3] == ['gh', 'secret', 'set']:
                calls.append(('upload', json.loads(data)))
                if fail_upload:
                    raise RuntimeError('gh failed; credentials and output withheld')
                return ''
            if args[:2] == ['gh', 'api']:
                return ''
            return real_run(args, data=data)

        env = {'LOCALCERT_CREDENTIALS': json.dumps({'subdomain': 'fixture', 'password': 'test-only'}),
               'APPLE_HLS_TLS_HOST': HOST, 'APPLE_HLS_TLS': json.dumps(current),
               'APPLE_HLS_ACME_ACCOUNT': json.dumps({'id': 'a' * 32, 'files': {n: {} for n in renewal.ACCOUNT_FILES}}),
               'GITHUB_REPOSITORY': 'fixture/repository'}
        with patch.dict(os.environ, env), patch.object(renewal, 'run', side_effect=run):
            renewal.renew()
        return calls

    def test_fresh_certificate_does_not_issue_or_update(self):
        self.assertEqual(self.exercise(self.fresh), [])

    def test_due_certificate_publishes_pair_in_one_update(self):
        self.assertEqual(self.exercise(self.due, self.fresh), [('issue', None), ('upload', self.fresh)])

    def test_short_lived_replacement_is_not_published(self):
        with self.assertRaisesRegex(RuntimeError, 'insufficient remaining validity'):
            self.exercise(self.due, self.due)

    def test_failed_secret_update_fails_the_job(self):
        with self.assertRaisesRegex(RuntimeError, 'gh failed'):
            self.exercise(self.due, self.fresh, fail_upload=True)

    def test_wrong_hostname_is_rejected(self):
        with self.assertRaises(RuntimeError):
            renewal.validate_pair(self.root / 'wrong-host', self.fresh, 'wrong.localcert.net')

    def test_mismatched_key_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'do not match'):
            renewal.validate_pair(self.root / 'wrong-key', dict(self.fresh, private_key=self.due['private_key']), HOST)

    def test_account_path_cannot_escape_private_directory(self):
        with self.assertRaisesRegex(RuntimeError, 'identifier'):
            renewal.restore_account(self.root, {'id': '../../elsewhere'})

    def test_dns_hook_refuses_other_identifiers_before_calling_api(self):
        env = {'LOCALCERT_CREDENTIALS': json.dumps({'subdomain': 'fixture', 'password': 'test-only'}),
               'APPLE_HLS_TLS_HOST': HOST, 'CERTBOT_IDENTIFIER': 'other.localcert.net'}
        with patch.dict(os.environ, env), patch.object(renewal.urllib.request, 'urlopen') as api:
            with self.assertRaisesRegex(RuntimeError, 'unexpected hostname'):
                renewal.dns_hook()
            api.assert_not_called()


if __name__ == '__main__':
    unittest.main()
