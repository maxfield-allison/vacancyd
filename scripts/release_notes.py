#!/usr/bin/env python3
"""Extract the exact version's reviewed changelog section for a release page."""
import argparse
from pathlib import Path
import re

try:
    from scripts.check_release_version import check
except ModuleNotFoundError:
    from check_release_version import check

ROOT = Path(__file__).resolve().parent.parent


def notes(root: Path, tag: str) -> str:
    """The dated changelog section for tag, after checking every shipped version."""
    version = check(root, tag)
    changelog = (root / 'CHANGELOG.md').read_text(encoding='utf-8')
    section = re.search(r'^## \[' + re.escape(version) + r'\] - [^\n]+\n(.*?)(?=^## |\Z)',
                        changelog, re.MULTILINE | re.DOTALL)
    if not section or not section.group(1).strip():
        raise ValueError('release notes must be nonempty')
    return section.group(1).strip() + '\n'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(notes(ROOT, args.tag), encoding='utf-8')
