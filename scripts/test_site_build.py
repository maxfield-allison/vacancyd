"""Exercise publication-sensitive website configuration and release discovery."""

import io
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch

import build_docs
import build_website
import site_config


class SiteBuildTests(unittest.TestCase):
    def test_docs_tracking_is_production_only_and_hostname_restricted(self):
        with tempfile.TemporaryDirectory() as directory:
            page = Path(directory) / "index.html"
            page.write_text("<html><head></head><body>Guide</body></html>")
            build_docs.add_analytics(Path(directory), "")
            self.assertNotIn("data-website-id", page.read_text())
            build_docs.add_analytics(
                Path(directory), "658e03cc-139a-4752-b140-01cfbf667f76"
            )
            document = page.read_text()
            for value in (
                'data-domains="docs.vacancyd.com"',
                'data-do-not-track="true"',
                'data-exclude-search="true"',
                'data-exclude-hash="true"',
            ):
                self.assertIn(value, document)
            with self.assertRaisesRegex(ValueError, "Unexpected analytics"):
                build_docs.add_analytics(
                    Path(directory), "658e03cc-139a-4752-b140-01cfbf667f76"
                )

    def test_previews_never_use_production_analytics_ids(self):
        self.assertEqual(site_config.analytics_id("app", True), "")
        self.assertEqual(site_config.tracker_tag("", "vacancyd.com"), "")

    def test_unpublished_releases_have_no_downloads(self):
        with patch(
            "urllib.request.urlopen",
            side_effect=urllib.error.HTTPError("url", 404, "not found", {}, None),
        ):
            self.assertEqual(
                build_website.release_info(False), {"available": False, "downloads": []}
            )

    def test_transient_release_failure_does_not_claim_no_release(self):
        with patch(
            "urllib.request.urlopen",
            side_effect=urllib.error.HTTPError("url", 503, "unavailable", {}, None),
        ), self.assertRaises(urllib.error.HTTPError):
            build_website.release_info(False)

    def test_incomplete_releases_fail_before_publishing_downloads(self):
        response = io.BytesIO(
            json.dumps(
                {
                    "draft": False,
                    "prerelease": False,
                    "tag_name": "v1.0.0",
                    "assets": [],
                }
            ).encode()
        )
        with (
            patch("urllib.request.urlopen", return_value=response),
            self.assertRaisesRegex(ValueError, "missing required asset"),
        ):
            build_website.release_info(False)


if __name__ == "__main__":
    unittest.main()
