#!/usr/bin/env python3
"""Build only the app site's public static files; fail on missing production analytics."""

from __future__ import annotations

import argparse
import json
import shutil
import urllib.error
import urllib.request

from site_config import ROOT, analytics_id, tracker_tag

OUTPUT = ROOT / "build" / "website"
RELEASE_API = "https://api.github.com/repos/maxfield-allison/vacancyd/releases/latest"


def release_info(preview: bool) -> dict:
    if preview:
        return {"available": False, "downloads": []}
    request = urllib.request.Request(
        RELEASE_API,
        headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "vacancyd-site-build",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            release = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return {"available": False, "downloads": []}
        raise
    if release["draft"] or release["prerelease"]:
        raise ValueError("The download page requires a published stable release")
    assets = {asset["name"]: asset for asset in release["assets"]}
    version = release["tag_name"].removeprefix("v")
    names = [
        (
            f"vacancyd-{version}-x86_64-pc-windows-msvc.msi",
            "Download for Windows",
            "download-windows",
        ),
        (
            f"vacancyd-{version}-x86_64-unknown-linux-gnu.tar.gz",
            "Download for Linux",
            "download-linux",
        ),
    ]
    for name in [*(row[0] for row in names), "SHA256SUMS", "SHA256SUMS.sigstore.json"]:
        if name not in assets:
            raise ValueError(f"Published release is missing required asset {name}")
    return {
        "available": True,
        "version": version,
        "downloads": [
            {
                "url": assets[name]["browser_download_url"],
                "label": label,
                "event": event,
            }
            for name, label, event in names
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--preview",
        action="store_true",
        help="local/PR preview without analytics or live release lookup",
    )
    args = parser.parse_args()
    identifier = analytics_id("app", args.preview)
    release = release_info(args.preview)
    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    for name in ["index.html", "styles.css", "app.js", "privacy/index.html"]:
        source = ROOT / "website" / name
        target = OUTPUT / name
        target.parent.mkdir(parents=True, exist_ok=True)
        contents = source.read_text().replace(
            "<!-- ANALYTICS -->", tracker_tag(identifier, "vacancyd.com")
        )
        target.write_text(contents)
    shutil.copytree(ROOT / "website" / "assets", OUTPUT / "assets")
    (OUTPUT / "assets" / "javascripts").mkdir(exist_ok=True)
    shutil.copyfile(
        ROOT / "website" / "analytics.js",
        OUTPUT / "assets" / "javascripts" / "analytics.js",
    )
    for directory in ("pricing", "rivet"):
        shutil.copytree(ROOT / "website" / directory, OUTPUT / directory)
    pricing = OUTPUT / "pricing" / "index.html"
    pricing.write_text(
        pricing.read_text().replace(
            "<!-- ANALYTICS -->", tracker_tag(identifier, "vacancyd.com")
        )
    )
    for name in ["mark-lit.svg", "mark-mono.svg"]:
        shutil.copyfile(
            ROOT / "docs" / "assets" / "brand" / name, OUTPUT / "assets" / name
        )
    shutil.copytree(
        ROOT / "console" / "web" / "assets" / "fonts", OUTPUT / "assets" / "fonts"
    )
    (OUTPUT / "release.json").write_text(json.dumps(release) + "\n")
    (OUTPUT / "robots.txt").write_text(
        "User-agent: *\nAllow: /\nSitemap: https://vacancyd.com/sitemap.xml\n"
    )
    (OUTPUT / "sitemap.xml").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>https://vacancyd.com/</loc></url><url><loc>https://vacancyd.com/pricing/</loc></url><url><loc>https://vacancyd.com/privacy/</loc></url></urlset>\n'
    )
    (OUTPUT / "_headers").write_text(
        "/*\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: strict-origin-when-cross-origin\n  Permissions-Policy: camera=(), microphone=(), geolocation=()\n  X-Frame-Options: DENY\n"
    )
    print(
        f"Built {OUTPUT.relative_to(ROOT)}; analytics={'preview-disabled' if not identifier else 'configured'}; downloads={release['available']}"
    )


if __name__ == "__main__":
    main()
