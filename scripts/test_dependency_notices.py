from copy import deepcopy
import json
import hashlib
import io
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from scripts.dependency_notices import render, source_archive, include_fonts


class DependencyNoticeTests(unittest.TestCase):
    def fixture(self):
        a = {"id": "local:a", "name": "a", "version": "1.0.0", "manifest_path": "/checkout/Cargo.toml"}
        b = {"id": "registry:b", "name": "b", "version": "2.0.0", "manifest_path": "/cache/b/Cargo.toml"}
        inventory = {"crates": [{"package": a, "license": "MIT"}, {"package": b, "license": "MIT"}],
                     "licenses": [{"id": "MIT", "source_path": "/cache/LICENSE", "text": "Copyright A\nPermission text\n",
                                   "used_by": [{"crate": a}, {"crate": b}]}]}
        metadata = {"resolve": {"nodes": [{"id": "local:a", "deps": [{"pkg": "registry:b"}]},
                                           {"id": "registry:b", "deps": []}]}}
        lock = {"package": [{"name": "a", "version": "1.0.0"},
                            {"name": "b", "version": "2.0.0", "checksum": "0" * 64}]}
        return inventory, metadata, lock

    def test_paths_and_input_order_do_not_change_output_or_lose_dependency_edges(self):
        inventory, metadata, lock = self.fixture()
        first = render(inventory, metadata, lock, "1" * 64)
        inventory["crates"].reverse()
        inventory["licenses"][0]["used_by"].reverse()
        inventory["licenses"][0]["source_path"] = "/another-cache/LICENSE"
        metadata["resolve"]["nodes"].reverse()
        second = render(inventory, metadata, lock, "1" * 64)
        self.assertEqual(first, second)
        self.assertNotIn("/cache/", first["SBOM.cdx.json"])
        self.assertIn("Copyright A", first["THIRD_PARTY_LICENSES"])
        bom = json.loads(first["SBOM.cdx.json"])
        self.assertEqual(bom["dependencies"][0], {"ref": "pkg:cargo/a@1.0.0", "dependsOn": ["pkg:cargo/b@2.0.0"]})
        self.assertEqual(bom["components"][1]["hashes"][0]["content"], "0" * 64)

    def test_missing_actual_text_or_coverage_cannot_produce_notices(self):
        original, metadata, lock = self.fixture()
        for field, value in (("source_path", None), ("text", ""), ("used_by", [])):
            inventory = deepcopy(original)
            inventory["licenses"][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                render(inventory, metadata, lock, "1" * 64)

    def test_mpl_sources_are_exact_locked_bytes_and_reproducible(self):
        inventory, _, lock = self.fixture()
        inventory["licenses"][0]["id"] = "MPL-2.0"
        inventory["licenses"][0]["used_by"] = inventory["licenses"][0]["used_by"][1:]
        contents = b"exact upstream archive, including its notices"
        lock["package"][1]["checksum"] = hashlib.sha256(contents).hexdigest()
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, CARGO_HOME=directory):
            cache = Path(directory) / "registry/cache/registry-id"
            cache.mkdir(parents=True)
            crate = cache / "b-2.0.0.crate"
            crate.write_bytes(contents)
            first = source_archive(inventory, lock)
            crate.touch()
            self.assertEqual(first, source_archive(inventory, lock))
            with tarfile.open(fileobj=io.BytesIO(first), mode="r:gz") as archive:
                self.assertEqual(archive.getnames(), [crate.name, "tauri-utils-2.9.3+vacancyd.1.crate"])
                patched = archive.extractfile("tauri-utils-2.9.3+vacancyd.1.crate").read()
                with tarfile.open(fileobj=io.BytesIO(patched), mode="r:gz") as source:
                    manifest = source.extractfile("tauri-utils-2.9.3+vacancyd.1/Cargo.toml").read()
                    self.assertIn(b'[dependencies.urlpattern]\nversion = "0.6"', manifest)
                self.assertEqual(archive.extractfile(crate.name).read(), contents)
            crate.write_bytes(b"modified source")
            with self.assertRaisesRegex(ValueError, "missing locked source"):
                source_archive(inventory, lock)

    def test_font_changes_and_unreviewed_licenses_fail(self):
        with tempfile.TemporaryDirectory() as directory, patch("scripts.dependency_notices.ROOT", Path(directory)):
            root = Path(directory)
            (root / "console").mkdir()
            (root / "about.toml").write_text('accepted = ["OFL-1.1"]')
            (root / "font.ttf").write_bytes(b"font")
            artifact = {"path": "font.ttf", "sha256": "0" * 64}
            font = {"name": "Font", "license": "OFL-1.1", "files": [artifact], "license_file": artifact}
            manifest = root / "console/font-licenses.json"
            manifest.write_text(json.dumps([font]))
            outputs = {"SBOM.cdx.json": '{"components": [], "dependencies": []}', "THIRD_PARTY_LICENSES": ""}
            with self.assertRaisesRegex(ValueError, "font artifact changed"):
                include_fonts(outputs)
            font["license"] = "Unreviewed"
            manifest.write_text(json.dumps([font]))
            with self.assertRaisesRegex(ValueError, "unreviewed font license"):
                include_fonts(outputs)


if __name__ == "__main__":
    unittest.main()
