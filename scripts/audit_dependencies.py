#!/usr/bin/env python3
"""Run RustSec over the whole lockfile; apply only exact, reviewed dispositions."""
from __future__ import annotations

import datetime as dt
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
TARGETS = ('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')


def reachable_packages(metadata: dict) -> set[tuple[str, str]]:
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    packages = {p['id']: (p['name'], p['version']) for p in metadata['packages']}
    pending = list(metadata['workspace_members'])
    visited = set()
    while pending:
        package = pending.pop()
        if package in visited:
            continue
        visited.add(package)
        pending.extend(dep['pkg'] for dep in nodes[package]['deps'])
    return {packages[p] for p in visited}


def assess(report: dict, reviews: list[dict], reachable: set[tuple[str, str]], today: dt.date) -> tuple[list[str], list[str]]:
    """All vulnerabilities and all unreviewed/new/moved warnings stay failures."""
    failures, dispositions = [], []
    if report['vulnerabilities']['found'] or report['vulnerabilities']['list']:
        failures.append('RustSec reports vulnerabilities; advisory review cannot waive them')
    entries = {}
    for review in reviews:
        key = (review['advisory'], review['package'], review['version'], review['warning'])
        if key in entries:
            raise ValueError(f'duplicate advisory review: {key}')
        entries[key] = review
    for kind, warnings in report['warnings'].items():
        for warning in warnings:
            advisory = warning.get('advisory') or {}
            package = warning['package']
            key = (advisory.get('id'), package['name'], package['version'], kind)
            label = f"{key[0]} {package['name']} {package['version']} ({kind})"
            review = entries.get(key)
            if review is None:
                failures.append(f'{label}: no exact review')
                continue
            if today >= dt.date.fromisoformat(review['expires']):
                failures.append(f'{label}: review expired {review["expires"]}')
                continue
            disposition = review['disposition']
            if disposition == 'outside-supported-graphs':
                if (package['name'], package['version']) in reachable:
                    failures.append(f'{label}: now reachable in a supported build')
                    continue
            else:
                failures.append(f'{label}: {disposition}')
                continue
            dispositions.append(f'{label}: {disposition}; review by {review["expires"]}')
    return failures, dispositions


def main() -> int:
    version = subprocess.run(['cargo', 'audit', '--version'], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()
    if version not in ('cargo-audit 0.22.2', 'cargo-audit-audit 0.22.2'):
        raise ValueError('Use pinned cargo-audit 0.22.2')
    audit = subprocess.run(['cargo', 'audit', '--json', '--deny', 'warnings'], cwd=ROOT, capture_output=True, text=True)
    # Preserve the unfiltered report, including rejected and accepted findings.
    destination = ROOT / 'target/audit-report.json'
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(audit.stdout)
    if audit.returncode not in (0, 1):
        raise ValueError(f'cargo-audit failed to run (exit {audit.returncode})')
    report = json.loads(audit.stdout)  # missing/invalid report fails closed
    reachable = set()
    for target in TARGETS:
        result = subprocess.run(['cargo', 'metadata', '--locked', '--offline', '--all-features',
                                 '--filter-platform', target, '--format-version', '1'],
                                cwd=ROOT, check=True, capture_output=True, text=True)
        reachable.update(reachable_packages(json.loads(result.stdout)))
    reviews = json.loads((ROOT / 'policy/advisory-reviews.json').read_text())['reviews']
    failures, dispositions = assess(report, reviews, reachable, dt.datetime.now(dt.timezone.utc).date())
    if audit.returncode and not dispositions and not failures:
        failures.append('cargo-audit failed without a dispositioned finding')
    for line in dispositions:
        print('REVIEWED:', line)
    for line in failures:
        print('BLOCKED:', line, file=sys.stderr)
    if failures:
        return 1
    print(f'RustSec gate passed; {len(dispositions)} exact reviewed warnings, full report: target/audit-report.json')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
