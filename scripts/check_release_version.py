#!/usr/bin/env python3
"""Fail before signing if any shipped version or release notes disagree."""
import json
import os
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def check(root: Path, tag: str) -> str:
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag):
        raise ValueError('release must run on a stable vX.Y.Z tag')
    version = tag[1:]
    for relative in ('Cargo.toml', 'console/Cargo.toml'):
        value = tomllib.loads((root / relative).read_text())['package']['version']
        if value != version:
            raise ValueError(f'{relative} differs from release tag')
    lock = tomllib.loads((root / 'Cargo.lock').read_text())
    packages = {package['name']: package['version'] for package in lock['package']
                if package['name'] in ('vacancyd', 'vacancyd-console')}
    if packages != {'vacancyd': version, 'vacancyd-console': version}:
        raise ValueError('workspace lock versions differ from release tag')
    for relative in ('console/tauri.conf.json', 'console/web/package.json', 'console/web/package-lock.json'):
        value = json.loads((root / relative).read_text())
        if value['version'] != version:
            raise ValueError(f'{relative} differs from release tag')
        if 'packages' in value and value['packages']['']['version'] != version:
            raise ValueError('frontend root lock version differs from release tag')
    changelog = (root / 'CHANGELOG.md').read_text()
    if not re.search(r'^## \[' + re.escape(version) + r'\] - \d{4}-\d{2}-\d{2}$', changelog, re.MULTILINE):
        raise ValueError('dated release notes are required')
    return version


if __name__ == '__main__':
    version = check(ROOT, os.environ['RELEASE_TAG'])
    print(f'Release versions and changelog agree: {version}')
