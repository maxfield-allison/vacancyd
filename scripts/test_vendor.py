from pathlib import Path
import shutil
import tempfile
import unittest

from scripts.check_vendor import ROOT, verify


class VendorIntegrityTests(unittest.TestCase):
    def test_only_the_recorded_patch_matches_the_upstream_archive(self):
        verify()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(ROOT / 'policy', root / 'policy')
            shutil.copytree(ROOT / 'vendor', root / 'vendor')
            source = root / 'vendor/tauri-utils/src/lib.rs'
            original = source.read_bytes()
            source.write_bytes(original + b'\n// unreviewed change\n')
            with self.assertRaisesRegex(ValueError, 'unreviewed vendored changes'):
                verify(root)
            source.write_bytes(original)
            (source.parent / 'unexpected.rs').write_text('// unexpected file')
            with self.assertRaisesRegex(ValueError, 'unreviewed vendored changes'):
                verify(root)

    def test_upstream_archive_cannot_be_replaced(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(ROOT / 'policy', root / 'policy')
            shutil.copytree(ROOT / 'vendor', root / 'vendor')
            archive = root / 'vendor/upstream/tauri-utils-2.9.3.crate'
            archive.write_bytes(archive.read_bytes() + b'changed')
            with self.assertRaisesRegex(ValueError, 'checksum changed'):
                verify(root)


if __name__ == '__main__':
    unittest.main()
