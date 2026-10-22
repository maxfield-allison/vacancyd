import json
from pathlib import Path
import tempfile
import unittest
from scripts.check_release_version import check


class ReleaseVersionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'console/web').mkdir(parents=True)
        for name in ('Cargo.toml', 'console/Cargo.toml'):
            (self.root / name).write_text('[package]\nversion = "1.0.0"\n')
        (self.root / 'Cargo.lock').write_text('\n'.join(
            f'[[package]]\nname = "{name}"\nversion = "1.0.0"\n'
            for name in ('vacancyd', 'vacancyd-console')))
        for name in ('console/tauri.conf.json', 'console/web/package.json', 'console/web/package-lock.json'):
            (self.root / name).write_text(json.dumps({'version': '1.0.0', 'packages': {'': {'version': '1.0.0'}}}))
        (self.root / 'CHANGELOG.md').write_text('## [1.0.0] - 2026-09-16\n')

    def test_consistent_candidate_passes(self):
        self.assertEqual(check(self.root, 'v1.0.0'), '1.0.0')

    def test_console_version_drift_blocks_signing(self):
        (self.root / 'console/Cargo.toml').write_text('[package]\nversion = "0.1.0"\n')
        with self.assertRaisesRegex(ValueError, 'console/Cargo.toml'):
            check(self.root, 'v1.0.0')

    def test_undated_notes_block_signing(self):
        (self.root / 'CHANGELOG.md').write_text('## [Unreleased]\n')
        with self.assertRaisesRegex(ValueError, 'dated release notes'):
            check(self.root, 'v1.0.0')

    def test_branch_or_prerelease_tag_refused(self):
        for tag in ('main', 'v1.0.0-rc1', '1.0.0', 'v1.0.0;false'):
            with self.assertRaises(ValueError):
                check(self.root, tag)
