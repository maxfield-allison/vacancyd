#!/usr/bin/env python3
"""Validate a complete signed release set and record its exact downloadable bytes.

This manifest is an inventory, not a cryptographic build attestation. The release
pipeline separately signs SHA256SUMS, which covers this manifest, and verifies
Windows trust on Windows. An unsigned set is accepted only for a rehearsal, and
its manifest says so.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import zipfile

TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-pc-windows-msvc')
NOTICES = ('THIRD_PARTY_LICENSES', 'SBOM.cdx.json', 'THIRD_PARTY_SOURCES.tar.gz')
EXES = ('vacancyd.exe', 'vacancyd-session.exe', 'vacancyd-console.exe')
OUTPUTS = {'release-manifest.json', 'SHA256SUMS', 'release-provenance.json', 'SHA256SUMS.sigstore.json'}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def inventory(directory: Path, tag: str, commit: str, repository: str,
              build_id: str, expected_subject: str | None, *, signed: bool = True) -> dict:
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', tag):
        raise ValueError('a stable vX.Y.Z tag is required')
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('a full source commit is required')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('invalid GitHub repository')
    if not re.fullmatch(r'[1-9][0-9]*', build_id):
        raise ValueError('the build pipeline number is required')
    if signed and not (expected_subject or '').strip():
        raise ValueError('the exact publisher subject is required')
    if not signed and expected_subject:
        raise ValueError('an unsigned rehearsal set names no publisher')
    version = tag[1:]
    archives = [f'vacancyd-{version}-{target}.' + ('zip' if 'windows' in target else 'tar.gz')
                for target in TARGETS]
    msi = f'vacancyd-{version}-x86_64-pc-windows-msvc.msi'
    packages = [*archives, msi]
    expected = set(packages + [name + '.sha256' for name in packages] + list(NOTICES))
    if signed:
        expected.add('windows-signatures.json')
    actual = {path.name for path in directory.iterdir()}
    if actual - OUTPUTS != expected:
        raise ValueError(f'release file set mismatch: missing={sorted(expected-actual)}, '
                         f'unexpected={sorted(actual-expected-OUTPUTS)}')
    for name in actual:
        path = directory / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f'regular release file required: {name}')
    hashes = {name: digest((directory / name).read_bytes()) for name in sorted(expected)}
    for name in packages:
        if (directory / (name + '.sha256')).read_bytes() != f'{hashes[name]}  {name}\n'.encode('ascii'):
            raise ValueError(f'checksum sidecar mismatch: {name}')
    by_name = check_signatures(directory, msi, hashes, expected_subject) if signed else None
    with zipfile.ZipFile(directory / archives[2]) as archive:
        prefix = f'vacancyd-{version}-x86_64-pc-windows-msvc/'
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise ValueError('duplicate ZIP entries')
        for member in archive.infolist():
            if (not member.filename.startswith(prefix) or '..' in Path(member.filename).parts
                    or '\\' in member.filename or member.is_dir()
                    or (member.external_attr >> 16) & 0o170000 == 0o120000):
                raise ValueError('unsafe Windows ZIP member')
        executable_names = {name for name in names if name.lower().endswith(('.exe', '.dll'))}
        if executable_names != {prefix + name for name in EXES}:
            raise ValueError('unexpected executable payload in Windows ZIP')
        for name in EXES:
            if by_name and digest(archive.read(prefix + name)) != by_name[name]['sha256']:
                raise ValueError(f'Windows ZIP differs from signed executable: {name}')
        for name in NOTICES:
            if digest(archive.read(prefix + name)) != hashes[name]:
                raise ValueError(f'Windows ZIP notice mismatch: {name}')
    for name, target in zip(archives[:2], TARGETS[:2]):
        prefix = f'vacancyd-{version}-{target}/'
        with tarfile.open(directory / name, 'r:gz') as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)):
                raise ValueError('duplicate tar entries')
            for member in members:
                if (not member.isfile() or not member.name.startswith(prefix)
                        or '..' in Path(member.name).parts):
                    raise ValueError('unsafe Linux archive member')
            if prefix + 'vacancyd' not in names or prefix + 'contrib/vacancyd.service' not in names:
                raise ValueError('missing Linux runtime payload')
            for notice in NOTICES:
                stream = archive.extractfile(prefix + notice)
                if stream is None or digest(stream.read()) != hashes[notice]:
                    raise ValueError(f'Linux archive notice mismatch: {notice}')
    return {
        'schema_version': 2, 'version': version, 'source_commit': commit,
        'repository': repository, 'tag': tag,
        'build_pipeline': int(build_id), 'signed': signed,
        'publisher_subject': expected_subject if signed else None,
        'artifacts': [{'file': name, 'sha256': hashes[name],
                       'bytes': (directory / name).stat().st_size} for name in sorted(expected)],
    }


def check_signatures(directory: Path, msi: str, hashes: dict, expected_subject: str) -> dict:
    receipts = json.loads((directory / 'windows-signatures.json').read_text(encoding='utf-8'))
    if not isinstance(receipts, list) or len(receipts) != 4:
        raise ValueError('four Windows signature receipts are required')
    by_name = {receipt['file']: receipt for receipt in receipts}
    if set(by_name) != {*EXES, msi}:
        raise ValueError('Windows signature receipt file set mismatch')
    for receipt in receipts:
        if (receipt.get('status') != 'Valid' or receipt.get('subject') != expected_subject
                or not receipt.get('timestamp_subject') or not receipt.get('signer_thumbprint')):
            raise ValueError('invalid or unexpected Windows signature receipt')
    if by_name[msi]['sha256'] != hashes[msi]:
        raise ValueError('MSI changed after signature verification')
    return by_name


def encode(manifest: dict) -> bytes:
    return (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()


def checksums(manifest: dict, extra: dict[str, bytes] | None = None) -> bytes:
    """SHA256SUMS over every artifact, the manifest and any extra release file."""
    files = {item['file']: item['sha256'] for item in manifest['artifacts']}
    files['release-manifest.json'] = digest(encode(manifest))
    for name, data in (extra or {}).items():
        if name in files:
            raise ValueError(f'duplicate release file {name}')
        files[name] = digest(data)
    return ''.join(f'{files[name]}  {name}\n' for name in sorted(files)).encode('ascii')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    for name in ('tag', 'commit', 'repository', 'build-id'):
        parser.add_argument('--' + name, required=True)
    signing = parser.add_mutually_exclusive_group(required=True)
    signing.add_argument('--expected-subject')
    signing.add_argument('--unsigned-rehearsal', action='store_true')
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    manifest = inventory(args.directory, args.tag, args.commit, args.repository,
                         args.build_id, args.expected_subject,
                         signed=not args.unsigned_rehearsal)
    outputs = {'release-manifest.json': encode(manifest), 'SHA256SUMS': checksums(manifest)}
    for name, content in outputs.items():
        destination = args.directory / name
        if args.check:
            if destination.read_bytes() != content:
                raise ValueError(f'changed release inventory: {name}')
        else:
            destination.write_bytes(content)
    kind = 'signed' if manifest['signed'] else 'unsigned rehearsal'
    print(f'Complete {kind} release inventory verified')


if __name__ == '__main__':
    main()
