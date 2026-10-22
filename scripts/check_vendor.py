#!/usr/bin/env python3
"""Verify the complete vendored tree against its locked upstream source archive."""
from __future__ import annotations

import hashlib
import io
import json
from pathlib import Path
import tarfile

ROOT = Path(__file__).resolve().parent.parent


def expected_files(archive: bytes, record: dict) -> dict[str, bytes]:
    if hashlib.sha256(archive).hexdigest() != record['sha256']:
        raise ValueError('upstream source archive checksum changed')
    files = {}
    prefix = record['upstream_package'] + '/'
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as source:
        for member in source.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or not member.name.startswith(prefix):
                raise ValueError('unexpected upstream archive entry')
            name = member.name[len(prefix):]
            if Path(name).is_absolute() or '..' in Path(name).parts:
                raise ValueError('unsafe upstream archive path')
            if name in files:
                raise ValueError('duplicate upstream archive entry')
            if name not in record['omitted_files']:
                files[name] = source.extractfile(member).read()
    for patch in record['replacements']:
        name, old, new = patch['path'], patch['before'].encode(), patch['after'].encode()
        if files[name].count(old) != 1:
            raise ValueError(f'patch no longer applies exactly once: {name}')
        files[name] = files[name].replace(old, new, 1)
    return files


def verify(root: Path = ROOT) -> None:
    record = json.loads((root / 'policy/vendor.json').read_text())
    expected = expected_files((root / record['archive']).read_bytes(), record)
    directory = root / record['directory']
    actual = {}
    for path in directory.rglob('*'):
        if path.is_symlink():
            raise ValueError('vendored source must not contain symlinks')
        if path.is_file():
            actual[path.relative_to(directory).as_posix()] = path.read_bytes()
    if actual != expected:
        changed = sorted(name for name in set(actual) | set(expected)
                         if actual.get(name) != expected.get(name))
        raise ValueError('unreviewed vendored changes: ' + ', '.join(changed))
    print(f'Vendored source verified: {len(expected)} files; only recorded upstream update and version label')


if __name__ == '__main__':
    verify()
