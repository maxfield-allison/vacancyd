#!/usr/bin/env python3
"""Build the documentation with the public Umami registration and strict validation."""

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

import prepare_docs
from site_config import ROOT, analytics_id, tracker_tag


def add_analytics(directory: Path, identifier: str) -> None:
    """Add hostname-restricted tracking only to an explicit production build."""
    tag = tracker_tag(identifier, "docs.vacancyd.com")
    if not tag:
        return
    for path in directory.rglob("*.html"):
        document = path.read_text(encoding="utf-8")
        if document.count("</head>") != 1 or "data-website-id=" in document:
            raise ValueError(f"Unexpected analytics/head markup in {path.name}")
        path.write_text(
            document.replace("</head>", tag + "\n</head>"), encoding="utf-8"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preview", action="store_true")
    args = parser.parse_args()
    identifier = analytics_id("docs", args.preview)
    prepare_docs.main()
    executable = Path(sys.executable).with_name("zensical")
    if not executable.is_file():
        located = shutil.which("zensical")
        if not located:
            raise RuntimeError("Install the hash-locked requirements-docs.txt first")
        executable = Path(located)
    subprocess.run(
        [
            str(executable),
            "build",
            "--config-file",
            "zensical.toml",
            "--clean",
            "--strict",
        ],
        cwd=ROOT,
        check=True,
    )
    add_analytics(ROOT / "site", identifier)


if __name__ == "__main__":
    main()
