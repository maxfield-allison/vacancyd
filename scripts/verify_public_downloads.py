#!/usr/bin/env python3
"""Download every release file anonymously and compare it to the qualified set."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import urllib.parse
import urllib.request

# Files that SHA256SUMS cannot list: itself and its signature bundle.
UNLISTED = {'SHA256SUMS', 'SHA256SUMS.sigstore.json'}


def download(reference: Path, output: Path, repository: str, tag: str,
             manifest_sha256: str) -> list[dict]:
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('invalid GitHub repository')
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag):
        raise ValueError('stable release tag required')
    if not re.fullmatch(r'[0-9a-f]{64}', manifest_sha256):
        raise ValueError('approved manifest SHA256 required')
    manifest = reference / 'release-manifest.json'
    if hashlib.sha256(manifest.read_bytes()).hexdigest() != manifest_sha256:
        raise ValueError('reference manifest differs from approval')
    files = sorted(reference.iterdir())
    sums = reference / 'SHA256SUMS'
    if not sums.is_file() or sums.is_symlink():
        raise ValueError('the qualified release directory needs its SHA256SUMS')
    listed = {line.split('  ', 1)[1] for line in sums.read_text(encoding='ascii').splitlines()}
    if {path.name for path in files} != listed | UNLISTED or 'release-manifest.json' not in listed:
        raise ValueError('reference files differ from SHA256SUMS and its signature')
    for path in files:
        if (path.is_symlink() or not path.is_file()
                or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', path.name)):
            raise ValueError('unsafe reference file')
    output.mkdir(parents=True, exist_ok=False)
    receipts = []
    for path in files:
        url = f'https://github.com/{repository}/releases/download/{tag}/{urllib.parse.quote(path.name)}'
        # urllib does not load GitHub CLI credentials or read GH_TOKEN.
        request = urllib.request.Request(url, headers={'User-Agent': 'Vacancyd-public-release-verifier'})
        hasher = hashlib.sha256()
        size = 0
        with urllib.request.urlopen(request, timeout=60) as response, (output / path.name).open('xb') as target:
            if urllib.parse.urlsplit(response.url).scheme != 'https':
                raise ValueError('download redirected away from HTTPS')
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > path.stat().st_size:
                    raise ValueError(f'public download exceeds qualified size: {path.name}')
                target.write(chunk)
                hasher.update(chunk)
        expected = hashlib.sha256(path.read_bytes()).hexdigest()
        if size != path.stat().st_size or hasher.hexdigest() != expected:
            raise ValueError(f'public download differs from qualified bytes: {path.name}')
        receipts.append({'file': path.name, 'url': url, 'bytes': size,
                         'sha256': expected, 'authenticated': False})
    return receipts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('reference', 'output', 'receipt'):
        parser.add_argument('--' + name, type=Path, required=True)
    for name in ('repository', 'tag', 'manifest-sha256'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args()
    receipts = download(args.reference, args.output, args.repository, args.tag, args.manifest_sha256)
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    args.receipt.write_text(json.dumps(receipts, indent=2) + '\n')
    print(f'All {len(receipts)} public download paths returned the qualified bytes without authentication')


if __name__ == '__main__':
    main()
