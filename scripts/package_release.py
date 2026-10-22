#!/usr/bin/env python3
"""Create a deterministic vacancyd release archive and SHA-256 sidecar."""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import hashlib
import io
import os
from pathlib import Path
import re
import tarfile
import tomllib
import zipfile


PROJECT_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_EPOCH = 315_532_800  # 1980-01-01, the earliest ZIP timestamp.
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$")


def project_version() -> str:
    with (PROJECT_ROOT / "Cargo.toml").open("rb") as stream:
        return tomllib.load(stream)["package"]["version"]


def normalized_expected_version(value: str) -> str:
    value = value.removeprefix("v")
    if not VERSION_RE.fullmatch(value):
        raise ValueError(f"invalid expected version: {value!r}")
    return value


def source_date_epoch() -> int:
    raw = os.environ.get("SOURCE_DATE_EPOCH")
    if raw is None:
        return DEFAULT_EPOCH
    try:
        value = int(raw)
    except ValueError as error:
        raise ValueError("SOURCE_DATE_EPOCH must be an integer") from error
    if value < DEFAULT_EPOCH:
        raise ValueError("SOURCE_DATE_EPOCH must be on or after 1980-01-01")
    return value


def payload(binary: Path, target: str) -> list[tuple[Path, str, int]]:
    executable_name = "vacancyd.exe" if "windows" in target else "vacancyd"
    files = [
        (binary, executable_name, 0o755),
        (PROJECT_ROOT / "README.md", "README.md", 0o644),
        (PROJECT_ROOT / "CHANGELOG.md", "CHANGELOG.md", 0o644),
        (PROJECT_ROOT / "SECURITY.md", "SECURITY.md", 0o644),
        (PROJECT_ROOT / "LICENSE-APACHE", "LICENSE-APACHE", 0o644),
        (PROJECT_ROOT / "NOTICE", "NOTICE", 0o644),
        (PROJECT_ROOT / "THIRD_PARTY_LICENSES", "THIRD_PARTY_LICENSES", 0o644),
        (PROJECT_ROOT / "SBOM.cdx.json", "SBOM.cdx.json", 0o644),
        (PROJECT_ROOT / "THIRD_PARTY_SOURCES.tar.gz", "THIRD_PARTY_SOURCES.tar.gz", 0o644),
        (PROJECT_ROOT / "vacancyd.example.toml", "vacancyd.example.toml", 0o644),
    ]
    files.extend(
        (document, document.relative_to(PROJECT_ROOT).as_posix(), 0o644)
        for document in sorted((PROJECT_ROOT / "docs").rglob("*"))
        if document.is_file()
    )
    if "windows" in target:
        files.extend(
            [
                (binary.with_name("vacancyd-session.exe"), "vacancyd-session.exe", 0o755),
                (binary.with_name("vacancyd-console.exe"), "vacancyd-console.exe", 0o755),
                (
                    PROJECT_ROOT / "packaging" / "vacancyd.starter.toml",
                    "vacancyd.starter.toml",
                    0o644,
                ),
            ]
        )
    elif "linux" in target:
        files.append(
            (
                PROJECT_ROOT / "contrib" / "vacancyd.service",
                "contrib/vacancyd.service",
                0o644,
            )
        )
    missing = [str(source) for source, _, _ in files if not source.is_file()]
    if missing:
        raise FileNotFoundError("missing release payload: " + ", ".join(missing))
    return files


def write_zip(
    destination: Path,
    prefix: str,
    files: list[tuple[Path, str, int]],
    epoch: int,
) -> None:
    timestamp = dt.datetime.fromtimestamp(epoch, tz=dt.timezone.utc)
    zip_time = (timestamp.year, timestamp.month, timestamp.day, timestamp.hour, timestamp.minute, timestamp.second)
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for source, relative, mode in files:
            info = zipfile.ZipInfo(f"{prefix}/{relative}", date_time=zip_time)
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, source.read_bytes(), compresslevel=9)


def write_tar_gz(
    destination: Path,
    prefix: str,
    files: list[tuple[Path, str, int]],
    epoch: int,
) -> None:
    with destination.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for source, relative, mode in files:
                    data = source.read_bytes()
                    info = tarfile.TarInfo(f"{prefix}/{relative}")
                    info.size = len(data)
                    info.mode = mode
                    info.mtime = epoch
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    archive.addfile(info, fileobj=io.BytesIO(data))


def sha256_sidecar(archive: Path) -> Path:
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    sidecar = archive.with_name(f"{archive.name}.sha256")
    sidecar.write_text(f"{digest}  {archive.name}\n", encoding="ascii", newline="\n")
    return sidecar


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="Rust target triple")
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-dir", type=Path, default=PROJECT_ROOT / "dist")
    parser.add_argument("--expected-version", help="Version or v-prefixed release tag")
    parser.add_argument("--format", choices=("auto", "zip", "tar.gz"), default="auto")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    version = project_version()
    if args.expected_version:
        expected = normalized_expected_version(args.expected_version)
        if expected != version:
            raise ValueError(
                f"release version mismatch: Cargo.toml has {version}, expected {expected}"
            )

    archive_format = args.format
    if archive_format == "auto":
        archive_format = "zip" if "windows" in args.target else "tar.gz"

    files = payload(args.binary.resolve(), args.target)
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    stem = f"vacancyd-{version}-{args.target}"
    suffix = ".zip" if archive_format == "zip" else ".tar.gz"
    archive = output_dir / f"{stem}{suffix}"
    epoch = source_date_epoch()

    if archive_format == "zip":
        write_zip(archive, stem, files, epoch)
    else:
        write_tar_gz(archive, stem, files, epoch)
    sidecar = sha256_sidecar(archive)
    print(archive)
    print(sidecar)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
