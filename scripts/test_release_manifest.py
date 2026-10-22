from __future__ import annotations

import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile

from scripts import release_manifest as release


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in release.NOTICES:
            (self.root / name).write_bytes(name.encode())
        receipts = []
        for name in release.EXES:
            receipts.append(self.receipt(name, name.encode()))
        for target in release.TARGETS:
            stem = f'vacancyd-1.0.0-{target}'
            data = {name: name.encode() for name in release.NOTICES}
            if 'windows' in target:
                data.update({name: name.encode() for name in release.EXES})
                path = self.root / (stem + '.zip')
                with zipfile.ZipFile(path, 'w') as archive:
                    for name, value in data.items():
                        archive.writestr(stem + '/' + name, value)
            else:
                data.update({'vacancyd': b'ELF', 'contrib/vacancyd.service': b'unit'})
                path = self.root / (stem + '.tar.gz')
                with tarfile.open(path, 'w:gz') as archive:
                    for name, value in data.items():
                        member = tarfile.TarInfo(stem + '/' + name)
                        member.size = len(value)
                        archive.addfile(member, io.BytesIO(value))
            self.sidecar(path)
        msi = self.root / 'vacancyd-1.0.0-x86_64-pc-windows-msvc.msi'
        msi.write_bytes(b'signed MSI')
        self.sidecar(msi)
        receipts.append(self.receipt(msi.name, msi.read_bytes()))
        (self.root / 'windows-signatures.json').write_text(json.dumps(receipts))

    def receipt(self, name, data):
        return {'file': name, 'sha256': release.digest(data), 'status': 'Valid',
                'subject': 'CN=Expected Publisher', 'timestamp_subject': 'CN=Timestamp',
                'signer_thumbprint': 'example'}

    def sidecar(self, path):
        path.with_name(path.name + '.sha256').write_text(
            f'{release.digest(path.read_bytes())}  {path.name}\n')

    def verify(self):
        return release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd',
                                 '123', 'CN=Expected Publisher')

    def test_complete_set_is_deterministic(self):
        self.assertEqual(self.verify(), self.verify())
        self.assertEqual(len(self.verify()['artifacts']), 12)

    def test_failure_fixture_cannot_escape_allowlist(self):
        (self.root / 'intentional-failure.msi').write_bytes(b'fixture')
        with self.assertRaisesRegex(ValueError, 'file set mismatch'):
            self.verify()

    def test_changed_msi_rejected_even_with_updated_checksum(self):
        msi = self.root / 'vacancyd-1.0.0-x86_64-pc-windows-msvc.msi'
        msi.write_bytes(b'changed after signing')
        self.sidecar(msi)
        with self.assertRaisesRegex(ValueError, 'MSI changed'):
            self.verify()

    def test_changed_zip_executable_rejected(self):
        path = self.root / 'vacancyd-1.0.0-x86_64-pc-windows-msvc.zip'
        with zipfile.ZipFile(path) as archive:
            contents = {name: archive.read(name) for name in archive.namelist()}
        name = next(name for name in contents if name.endswith('/vacancyd.exe'))
        contents[name] = b'changed after signing'
        with zipfile.ZipFile(path, 'w') as archive:
            for name, value in contents.items():
                archive.writestr(name, value)
        self.sidecar(path)
        with self.assertRaisesRegex(ValueError, 'differs from signed'):
            self.verify()

    def test_unexpected_publisher_rejected(self):
        path = self.root / 'windows-signatures.json'
        receipts = json.loads(path.read_text())
        receipts[0]['subject'] = 'CN=Different Publisher'
        path.write_text(json.dumps(receipts))
        with self.assertRaisesRegex(ValueError, 'unexpected Windows signature'):
            self.verify()

    def test_mismatched_notices_rejected(self):
        (self.root / 'SBOM.cdx.json').write_bytes(b'new inventory')
        with self.assertRaisesRegex(ValueError, 'notice mismatch'):
            self.verify()

    def test_missing_or_stale_checksum_rejected(self):
        path = self.root / 'vacancyd-1.0.0-aarch64-unknown-linux-gnu.tar.gz.sha256'
        path.write_bytes(b'0' * 64)
        with self.assertRaisesRegex(ValueError, 'checksum sidecar mismatch'):
            self.verify()

    def test_symbolic_link_rejected(self):
        path = self.root / 'THIRD_PARTY_LICENSES'
        path.unlink()
        path.symlink_to('SBOM.cdx.json')
        with self.assertRaisesRegex(ValueError, 'regular release file'):
            self.verify()


    def test_manifest_names_the_build_pipeline_not_a_private_url(self):
        manifest = self.verify()
        self.assertEqual(manifest['schema_version'], 2)
        self.assertEqual(manifest['build_pipeline'], 123)
        self.assertTrue(manifest['signed'])
        self.assertNotIn('build_run', manifest)
        self.assertEqual(manifest['publisher_subject'], 'CN=Expected Publisher')

    def test_unsigned_rehearsal_needs_no_receipts_and_says_so(self):
        (self.root / 'windows-signatures.json').unlink()
        manifest = release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd', '123',
                                     None, signed=False)
        self.assertFalse(manifest['signed'])
        self.assertIsNone(manifest['publisher_subject'])
        self.assertEqual(len(manifest['artifacts']), 11)

    def test_unsigned_rehearsal_refuses_a_stray_signature_receipt(self):
        with self.assertRaisesRegex(ValueError, 'file set mismatch'):
            release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd', '123',
                              None, signed=False)

    def test_signing_mode_and_subject_must_agree(self):
        with self.assertRaisesRegex(ValueError, 'publisher subject is required'):
            release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd', '123', '')
        with self.assertRaisesRegex(ValueError, 'names no publisher'):
            release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd', '123',
                              'CN=Expected Publisher', signed=False)

    def test_missing_signature_receipt_fails_a_signed_set(self):
        (self.root / 'windows-signatures.json').unlink()
        with self.assertRaisesRegex(ValueError, 'file set mismatch'):
            self.verify()

    def test_build_pipeline_must_be_a_positive_number(self):
        for value in ('0', '', 'https://example.test/pipelines/1', '12a'):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'pipeline number'):
                release.inventory(self.root, 'v1.0.0', 'a' * 40, 'example/vacancyd', value,
                                  'CN=Expected Publisher')

    def test_checksums_cover_manifest_and_extras_and_refuse_duplicates(self):
        manifest = self.verify()
        sums = release.checksums(manifest, {'release-provenance.json': b'{}'}).decode()
        names = [line.split('  ', 1)[1] for line in sums.splitlines()]
        self.assertEqual(names, sorted(names))
        self.assertIn('release-manifest.json', names)
        self.assertIn('release-provenance.json', names)
        self.assertEqual(len(names), 14)
        with self.assertRaisesRegex(ValueError, 'duplicate release file'):
            release.checksums(manifest, {'SBOM.cdx.json': b'x'})


if __name__ == '__main__':
    unittest.main()
