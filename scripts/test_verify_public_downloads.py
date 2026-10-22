import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from scripts.verify_public_downloads import download


class PublicDownloadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.reference = self.root / 'reference'
        self.reference.mkdir()
        for i in range(13):
            (self.reference / f'asset-{i}.zip').write_bytes(f'payload-{i}'.encode())
        (self.reference / 'release-manifest.json').write_text(json.dumps({'test': True}))
        self.sha = hashlib.sha256((self.reference / 'release-manifest.json').read_bytes()).hexdigest()
        self.write_sums()
        (self.reference / 'SHA256SUMS.sigstore.json').write_text('{}')

    def write_sums(self):
        lines = [f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
                 for path in sorted(self.reference.iterdir())
                 if path.name not in ('SHA256SUMS', 'SHA256SUMS.sigstore.json')]
        (self.reference / 'SHA256SUMS').write_text(''.join(lines))

    def verify(self):
        return download(self.reference, self.root / 'downloaded', 'example/vacancyd', 'v1.0.0', self.sha)

    def response(self, request, timeout):
        self.assertEqual(timeout, 60)
        self.assertNotIn('Authorization', request.headers)
        self.assertTrue(request.full_url.startswith('https://github.com/example/vacancyd/releases/download/v1.0.0/'))
        name = request.full_url.rsplit('/', 1)[1]
        response = io.BytesIO((self.reference / name).read_bytes())
        response.url = 'https://release-assets.githubusercontent.com/example'
        return response

    def test_all_final_paths_verified_without_credentials(self):
        with patch('scripts.verify_public_downloads.urllib.request.urlopen', side_effect=self.response) as opener:
            result = self.verify()
        self.assertEqual(opener.call_count, 16)
        self.assertTrue(all(item['authenticated'] is False for item in result))

    def test_bad_reference_never_starts_network_requests(self):
        self.sha = '0' * 64
        with patch('scripts.verify_public_downloads.urllib.request.urlopen') as opener:
            with self.assertRaisesRegex(ValueError, 'differs from approval'):
                self.verify()
            opener.assert_not_called()

    def test_changed_download_is_rejected(self):
        def changed(request, timeout):
            response = self.response(request, timeout)
            response.seek(0)
            response.write(b'!')
            response.seek(0)
            return response
        with patch('scripts.verify_public_downloads.urllib.request.urlopen', side_effect=changed):
            with self.assertRaisesRegex(ValueError, 'differs from qualified bytes'):
                self.verify()

    def test_oversized_download_stops_at_qualified_size(self):
        def oversized(request, timeout):
            response = self.response(request, timeout)
            response.seek(0, 2)
            response.write(b'extra')
            response.seek(0)
            return response
        with patch('scripts.verify_public_downloads.urllib.request.urlopen', side_effect=oversized):
            with self.assertRaisesRegex(ValueError, 'exceeds qualified size'):
                self.verify()

    def test_reference_must_match_its_checksum_list(self):
        (self.reference / 'intentional-failure.msi').write_bytes(b'fixture')
        with patch('scripts.verify_public_downloads.urllib.request.urlopen') as opener:
            with self.assertRaisesRegex(ValueError, 'differ from SHA256SUMS'):
                self.verify()
            opener.assert_not_called()

    def test_missing_checksum_list_is_refused(self):
        (self.reference / 'SHA256SUMS').unlink()
        with self.assertRaisesRegex(ValueError, 'needs its SHA256SUMS'):
            self.verify()
