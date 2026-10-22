#!/usr/bin/env python3
"""Stage, identify and publish the vacancyd container image without rebuilding it.

inputs   Copy the two Linux release archives and their .sha256 sidecars into
         a new build context, checking each archive against its sidecar, and
         print the staged archives' SHA-256 digests. The release pipeline
         compares those digests with the release manifest before publishing.
digests  Read the image index digest and per-platform manifest digests from
         the OCI archive the build produced, verifying each blob it reads.
verify   Prove what each platform image in the OCI archive holds, without
         trusting the job that built it: its layers are the pinned base image's
         layers, fetched from the base registry by digest, plus one layer holding
         exactly the files of that platform's release archive, and its
         configuration is the one the Dockerfile declares. Prints each
         platform's manifest and configuration digests and layer diff IDs.
publish  Copy that exact OCI archive to each registry with its digests
         preserved: the immutable vX.Y.Z tag first, then `latest` when asked.
         An existing version tag with other content stops publication.

Registry credentials come from the auth file named by REGISTRY_AUTH_FILE,
never from arguments. Every copied reference is read back and compared. A
release rehearsal publishes to a throwaway registry instead, named with
--rehearsal-registry; only that host may then be written, over plain HTTP.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.error
import urllib.parse
import urllib.request

ARCHES = {"amd64": "x86_64-unknown-linux-gnu", "arm64": "aarch64-unknown-linux-gnu"}
TAG = re.compile(r"v([0-9]+\.[0-9]+\.[0-9]+)\Z")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+\Z")
REHEARSAL_HOST = re.compile(r"[a-z0-9]+(?:[.-][a-z0-9]+)*:[0-9]{2,5}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
REPOSITORY = re.compile(r"(docker\.io|ghcr\.io)/[a-z0-9]+(?:[._-][a-z0-9]+)*/vacancyd\Z")
INDEX_TYPES = {"application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json"}
MANIFEST_TYPES = {"application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"}
DOCKERFILE = Path(__file__).resolve().parent.parent / "packaging" / "container" / "Dockerfile"
PINNED_IMAGE = re.compile(r"(?P<registry>[a-z0-9.-]+\.[a-z]+(?::[0-9]+)?)/(?P<repository>[a-z0-9]+(?:[._/-][a-z0-9]+)*)"
                          r"(?::[A-Za-z0-9_][A-Za-z0-9_.-]*)?@(?P<digest>sha256:[0-9a-f]{64})\Z")
# What packaging/container/Dockerfile declares on top of the base image. A test
# reads the Dockerfile to keep the two in step.
USER = "65532:65532"
ENTRYPOINT = ["/usr/local/bin/vacancyd"]
CMD = ["run", "--config", "/etc/vacancyd/vacancyd.toml"]
HEALTHCHECK = ["CMD", "/usr/local/bin/vacancyd", "healthcheck", "--config", "/etc/vacancyd/vacancyd.toml"]
ENV = ["STATE_DIRECTORY=/var/lib/vacancyd", "NVIDIA_DRIVER_CAPABILITIES=utility",
       "DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket"]
PORTS = {"9811/tcp"}
SERVICE_ID = 65532
# Directories the payload stage creates, with owner and mode.
PAYLOAD_DIRECTORIES = {
    "etc": (0, 0o755), "etc/vacancyd": (0, 0o755), "run": (0, 0o755), "run/vacancyd": (SERVICE_ID, 0o755),
    "usr": (0, 0o755), "usr/local": (0, 0o755), "usr/local/bin": (0, 0o755), "usr/share": (0, 0o755),
    "usr/share/doc": (0, 0o755), "usr/share/doc/vacancyd": (0, 0o755), "var": (0, 0o755),
    "var/lib": (0, 0o755), "var/lib/vacancyd": (SERVICE_ID, 0o700),
}


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def stage_inputs(directory: Path, output: Path, version: str) -> dict[str, str]:
    """Copy both Linux archives and their sidecars; return {archive: sha256}."""
    if not VERSION.fullmatch(version):
        raise ValueError("the archive version is X.Y.Z, without v")
    if output.exists():
        raise ValueError(f"build context {output} must be new")
    staged = {}
    for target in ARCHES.values():
        archive = f"vacancyd-{version}-{target}.tar.gz"
        for name in (archive, archive + ".sha256"):
            source = directory / name
            if source.is_symlink() or not source.is_file():
                raise ValueError(f"missing release file {name}")
        actual = hashlib.sha256((directory / archive).read_bytes()).hexdigest()
        if (directory / (archive + ".sha256")).read_bytes() != f"{actual}  {archive}\n".encode("ascii"):
            raise ValueError(f"{archive} does not match its .sha256 sidecar")
        staged[archive] = actual
    output.mkdir(parents=True)
    for archive in staged:
        for name in (archive, archive + ".sha256"):
            shutil.copyfile(directory / name, output / name)
    return staged


def read_member(archive: tarfile.TarFile, name: str) -> bytes:
    member = archive.getmember(name)
    if not member.isfile():
        raise ValueError(f"{name} is not a regular file in the OCI archive")
    stream = archive.extractfile(member)
    assert stream is not None
    return stream.read()


def blob(archive: tarfile.TarFile, digest: str) -> bytes:
    if not DIGEST.fullmatch(digest):
        raise ValueError(f"invalid digest {digest!r}")
    data = read_member(archive, "blobs/sha256/" + digest.split(":", 1)[1])
    if sha256(data) != digest:
        raise ValueError(f"OCI blob {digest} is corrupt")
    return data


def image_digests(path: Path) -> dict:
    """The index digest and the digest of each expected platform manifest."""
    with tarfile.open(path) as archive:
        layout = json.loads(read_member(archive, "index.json"))
        entries = layout.get("manifests") or []
        if len(entries) != 1 or entries[0].get("mediaType") not in INDEX_TYPES:
            raise ValueError("OCI archive must hold exactly one multi-platform image index")
        index_digest = entries[0]["digest"]
        index = json.loads(blob(archive, index_digest))
        platforms = {}
        for entry in index.get("manifests") or []:
            platform = entry.get("platform") or {}
            if platform.get("os") == "unknown":
                continue
            key = f"{platform.get('os')}/{platform.get('architecture')}"
            if key in platforms:
                raise ValueError(f"duplicate platform {key}")
            manifest = json.loads(blob(archive, entry["digest"]))
            for layer in [manifest["config"], *manifest["layers"]]:
                blob(archive, layer["digest"])
            platforms[key] = entry["digest"]
    wanted = {f"linux/{arch}" for arch in ARCHES}
    if set(platforms) != wanted:
        raise ValueError(f"image platforms {sorted(platforms)}, expected {sorted(wanted)}")
    return {"index": index_digest, "platforms": platforms}


def runtime_base(dockerfile: Path = DOCKERFILE) -> str:
    """The runtime base image the Dockerfile pins, as registry/repository[:tag]@digest."""
    found = re.findall(r"^ARG RUNTIME_IMAGE=(\S+)$", dockerfile.read_text(encoding="utf-8"), re.MULTILINE)
    if len(found) != 1 or not PINNED_IMAGE.fullmatch(found[0]):
        raise ValueError("the Dockerfile must pin exactly one RUNTIME_IMAGE by digest")
    return found[0]


def dockerfile_labels(values: dict[str, str], dockerfile: Path = DOCKERFILE) -> dict[str, str]:
    """The image labels the Dockerfile sets, with its build arguments substituted."""
    text = dockerfile.read_text(encoding="utf-8")
    labels = dict(re.findall(r'(org\.opencontainers\.image\.[a-z.]+)="([^"]*)"', text))
    if not labels:
        raise ValueError("the Dockerfile sets no image labels")

    def substitute(value: str) -> str:
        def argument(match: re.Match) -> str:
            if match.group(1) not in values:
                raise ValueError(f"no value for build argument {match.group(1)}")
            return values[match.group(1)]
        return re.sub(r"\$\{(\w+)\}", argument, value)
    return {name: substitute(value) for name, value in labels.items()}


class _DropAuthorizationOnRedirect(urllib.request.HTTPRedirectHandler):
    """Registries redirect blob reads to storage that rejects the registry's token."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None and urllib.parse.urlsplit(newurl).netloc != urllib.parse.urlsplit(req.full_url).netloc:
            new.remove_header("Authorization")
        return new


class BaseImages:
    """Anonymous, digest-verified reads of a public base image."""

    ACCEPT = ", ".join(sorted(INDEX_TYPES | MANIFEST_TYPES))

    def __init__(self) -> None:
        self.opener = urllib.request.build_opener(_DropAuthorizationOnRedirect)
        self.tokens: dict[str, str] = {}

    def read(self, registry: str, repository: str, kind: str, digest: str) -> bytes:
        if not DIGEST.fullmatch(digest):
            raise ValueError(f"invalid digest {digest!r}")
        data = self.fetch(registry, repository, f"{kind}/{digest}")
        if sha256(data) != digest:
            raise ValueError(f"{registry}/{repository} served other content for {digest}")
        return data

    def resolve(self, registry: str, repository: str, tag: str) -> str:
        """The digest of the manifest `tag` names now, read anonymously."""
        if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag):
            raise ValueError(f"invalid tag {tag!r}")
        return sha256(self.fetch(registry, repository, f"manifests/{tag}"))

    def fetch(self, registry: str, repository: str, path: str) -> bytes:
        url = f"https://{registry}/v2/{repository}/{path}"
        for attempt in range(2):
            headers = {"Accept": self.ACCEPT}
            if repository in self.tokens:
                headers["Authorization"] = "Bearer " + self.tokens[repository]
            try:
                with self.opener.open(urllib.request.Request(url, headers=headers), timeout=120) as response:
                    return response.read()
            except urllib.error.HTTPError as error:
                if error.code != 401 or attempt:
                    raise RuntimeError(f"reading {registry}/{repository} {path} failed: HTTP {error.code}") from None
                self.tokens[repository] = self.anonymous_token(error.headers.get("WWW-Authenticate", ""))
        raise AssertionError("unreachable")

    def anonymous_token(self, challenge: str) -> str:
        if not challenge.startswith("Bearer "):
            raise RuntimeError("the base registry asks for credentials this check does not have")
        fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        realm = urllib.parse.urlsplit(fields.get("realm", ""))
        if realm.scheme != "https":
            raise RuntimeError("the base registry's token service is not HTTPS")
        query = urllib.parse.urlencode({key: fields[key] for key in ("service", "scope") if key in fields})
        with self.opener.open(urllib.request.Request(realm.geturl() + "?" + query), timeout=60) as response:
            body = json.load(response)
        token = body.get("token") or body.get("access_token")
        if not token:
            raise RuntimeError("the base registry issued no anonymous token")
        return token

    def config(self, reference: str, architecture: str) -> dict:
        """The configuration of `reference`'s linux/<architecture> image."""
        match = PINNED_IMAGE.fullmatch(reference)
        if not match:
            raise ValueError(f"{reference!r} is not pinned by digest")
        registry, repository = match["registry"], match["repository"]
        document = json.loads(self.read(registry, repository, "manifests", match["digest"]))
        if document.get("mediaType") in INDEX_TYPES or "manifests" in document:
            chosen = [entry for entry in document.get("manifests") or []
                      if (entry.get("platform") or {}).get("os") == "linux"
                      and (entry.get("platform") or {}).get("architecture") == architecture]
            if len(chosen) != 1:
                raise ValueError(f"{reference} has no single linux/{architecture} image")
            document = json.loads(self.read(registry, repository, "manifests", chosen[0]["digest"]))
        return json.loads(self.read(registry, repository, "blobs", document["config"]["digest"]))


def layer_tar(data: bytes, media_type: str) -> bytes:
    if media_type.endswith("+gzip") or media_type.endswith(".gzip"):
        return gzip.decompress(data)
    if media_type.endswith(".tar") or media_type.endswith("layer.v1.tar"):
        return data
    raise ValueError(f"unsupported layer type {media_type}")


def expected_payload(archive_path: Path, stem: str) -> tuple[dict, dict]:
    """{path: (bytes, mode)} and {directory: (owner, mode)} the payload stage makes from an archive."""
    files: dict[str, tuple[bytes, int]] = {}
    directories = dict(PAYLOAD_DIRECTORIES)
    if archive_path.is_symlink() or not archive_path.is_file():
        raise ValueError(f"missing release archive {archive_path.name}")
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive.getmembers():
            name = member.name.rstrip("/")
            if member.isdir() and name == stem:
                continue
            if not name.startswith(stem + "/") or ".." in PurePosixPath(name).parts:
                raise ValueError(f"{archive_path.name} holds {member.name!r} outside {stem}/")
            relative = name[len(stem) + 1:]
            if member.isdir():
                directories["usr/share/doc/vacancyd/" + relative] = (0, 0o755)
                continue
            if not member.isfile():
                raise ValueError(f"{archive_path.name} holds a non-regular entry {member.name!r}")
            stream = archive.extractfile(member)
            assert stream is not None
            data = stream.read()
            if relative == "vacancyd":
                files["usr/local/bin/vacancyd"] = (data, 0o755)
                continue
            target = "usr/share/doc/vacancyd/" + relative
            files[target] = (data, 0o755 if member.mode & 0o111 else 0o644)
            parent = PurePosixPath(target).parent
            while str(parent) != "usr/share/doc/vacancyd":
                directories[str(parent)] = (0, 0o755)
                parent = parent.parent
    if "usr/local/bin/vacancyd" not in files:
        raise ValueError(f"{archive_path.name} holds no vacancyd binary")
    return files, directories


def payload_problems(layer: bytes, files: dict, directories: dict) -> list[str]:
    problems, seen = [], set()
    with tarfile.open(fileobj=io.BytesIO(layer)) as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix("./").rstrip("/")
            if not name and member.isdir():
                continue
            if name in seen:
                problems.append(f"{name} appears twice")
            seen.add(name)
            if PurePosixPath(name).name.startswith(".wh."):
                problems.append(f"{name} deletes a base image file")
            elif member.isdir():
                if name not in directories:
                    problems.append(f"unexpected directory {name}")
                elif (member.uid, member.gid, member.mode & 0o7777) != (directories[name][0], directories[name][0], directories[name][1]):
                    problems.append(f"{name} has owner {member.uid}:{member.gid} mode {member.mode & 0o7777:o}")
            elif member.isfile():
                if name not in files:
                    problems.append(f"unexpected file {name}")
                    continue
                stream = archive.extractfile(member)
                assert stream is not None
                data, mode = files[name]
                if stream.read() != data:
                    problems.append(f"{name} differs from the release archive")
                if (member.uid, member.gid, member.mode & 0o7777) != (0, 0, mode):
                    problems.append(f"{name} has owner {member.uid}:{member.gid} mode {member.mode & 0o7777:o}")
            else:
                problems.append(f"{name} is not a regular file or directory")
    for name in sorted((set(files) | set(directories)) - seen):
        problems.append(f"{name} is missing")
    return problems


def configuration_problems(config: dict, base: dict, labels: dict[str, str], architecture: str) -> list[str]:
    problems = []
    if (config.get("os"), config.get("architecture")) != ("linux", architecture):
        problems.append(f"the configuration is for {config.get('os')}/{config.get('architecture')}")
    settings, inherited = config.get("config") or {}, base.get("config") or {}
    expected = {"User": USER, "Entrypoint": ENTRYPOINT, "Cmd": CMD,
                "Env": list(inherited.get("Env") or []) + ENV,
                "Labels": {**(inherited.get("Labels") or {}), **labels}}
    for key, value in expected.items():
        if settings.get(key) != value:
            problems.append(f"{key} is {settings.get(key)!r}, expected {value!r}")
    if set(settings.get("ExposedPorts") or {}) != set(inherited.get("ExposedPorts") or {}) | PORTS:
        problems.append(f"exposed ports are {sorted(settings.get('ExposedPorts') or {})}")
    if (settings.get("Healthcheck") or {}).get("Test") != HEALTHCHECK:
        problems.append("the health check is not vacancyd's")
    for key in ("WorkingDir", "Volumes", "StopSignal", "OnBuild", "Shell"):
        if (settings.get(key) or None) != (inherited.get(key) or None):
            problems.append(f"{key} differs from the base image")
    return problems


def verify_image(archive_path: Path, directory: Path, version: str, revision: str, created: str,
                 base_config=None, dockerfile: Path = DOCKERFILE) -> dict:
    """Each platform image is the pinned base plus exactly its release archive's files.

    Returns {platform: {"manifest", "config", "diff_ids"}}; raises ValueError
    naming every difference otherwise. `base_config(reference, architecture)`
    reads the base image's configuration (by default from its registry).
    """
    if not VERSION.fullmatch(version):
        raise ValueError("the archive version is X.Y.Z, without v")
    base_reference = runtime_base(dockerfile)
    labels = dockerfile_labels({"VERSION": version, "REVISION": revision, "CREATED": created,
                                "RUNTIME_IMAGE": base_reference}, dockerfile)
    base_config = base_config or BaseImages().config
    digests = image_digests(archive_path)
    identities, problems = {}, []
    with tarfile.open(archive_path) as archive:
        for architecture, target in ARCHES.items():
            platform = f"linux/{architecture}"
            manifest_digest = digests["platforms"][platform]
            manifest = json.loads(blob(archive, manifest_digest))
            config_digest = manifest["config"]["digest"]
            config = json.loads(blob(archive, config_digest))
            diff_ids = list((config.get("rootfs") or {}).get("diff_ids") or [])
            layers = manifest.get("layers") or []
            if len(layers) != len(diff_ids):
                problems.append(f"{platform}: {len(layers)} layers but {len(diff_ids)} diff IDs")
                continue
            tars = [layer_tar(blob(archive, layer["digest"]), layer.get("mediaType", "")) for layer in layers]
            for index, data in enumerate(tars):
                if sha256(data) != diff_ids[index]:
                    problems.append(f"{platform}: layer {index} does not match its diff ID")
            base = base_config(base_reference, architecture)
            base_ids = list((base.get("rootfs") or {}).get("diff_ids") or [])
            if diff_ids[:-1] != base_ids or not base_ids:
                problems.append(f"{platform}: the layers below the payload are not {base_reference}")
            stem = f"vacancyd-{version}-{target}"
            files, directories = expected_payload(directory / f"{stem}.tar.gz", stem)
            problems += [f"{platform}: {problem}" for problem in payload_problems(tars[-1], files, directories)]
            problems += [f"{platform}: {problem}" for problem in configuration_problems(config, base, labels, architecture)]
            identities[platform] = {"manifest": manifest_digest, "config": config_digest, "diff_ids": diff_ids}
    if problems:
        raise ValueError("the image is not the release archives on the pinned base:\n  " + "\n  ".join(problems))
    return identities


class Registry:
    """skopeo, with credentials from REGISTRY_AUTH_FILE only."""

    def __init__(self, tls_verify: bool = True) -> None:
        self.tls = [] if tls_verify else ["--tls-verify=false"]
        self.dest_tls = [] if tls_verify else ["--dest-tls-verify=false"]

    def run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        result = subprocess.run(["skopeo", *args], text=True, capture_output=True, timeout=1800)
        if check and result.returncode:
            # skopeo errors can quote server text; keep the operation and status.
            raise RuntimeError(f"skopeo {args[0]} failed (exit {result.returncode})")
        return result

    def digest(self, reference: str) -> str | None:
        """The manifest digest at `reference`, or None when it does not exist."""
        result = self.run("inspect", *self.tls, "--raw", "docker://" + reference, check=False)
        if result.returncode == 0:
            return sha256(result.stdout.encode())
        # Only an explicit absence permits creation. Authentication and network
        # errors are not evidence that a tag is free.
        if any(marker in result.stderr for marker in ("manifest unknown", "MANIFEST_UNKNOWN", "name unknown", "NAME_UNKNOWN")):
            return None
        raise RuntimeError(f"cannot establish whether {reference} exists")

    def copy(self, source: str, reference: str) -> None:
        flags = list(self.dest_tls)
        if self.dest_tls and source.startswith("docker://"):
            flags.append("--src-tls-verify=false")  # moving latest within the rehearsal registry
        self.run("copy", *flags, "--all", "--preserve-digests", "--retry-times", "3", source, "docker://" + reference)


def allowed(repository: str, rehearsal_host: str | None) -> bool:
    """A public registry name, or in a rehearsal only the throwaway registry's."""
    if rehearsal_host is None:
        return bool(REPOSITORY.fullmatch(repository))
    host, _, path = repository.partition("/")
    return host == rehearsal_host and bool(REPOSITORY.fullmatch("docker.io/" + path))


def publish(archive: Path, expected: str, repositories: list[str], tag: str, latest: bool, registry,
            rehearsal_host: str | None = None) -> list[dict]:
    if not TAG.fullmatch(tag):
        raise ValueError("a stable vX.Y.Z tag is required")
    if not DIGEST.fullmatch(expected):
        raise ValueError("the qualified index digest is required")
    if rehearsal_host is not None and not REHEARSAL_HOST.fullmatch(rehearsal_host):
        raise ValueError("the rehearsal registry is a host:port")
    for repository in repositories:
        if not allowed(repository, rehearsal_host):
            raise ValueError(f"unexpected repository {repository!r}")
    if image_digests(archive)["index"] != expected:
        raise ValueError("OCI archive is not the qualified image")
    receipt = []
    for repository in repositories:
        reference = f"{repository}:{tag}"
        existing = registry.digest(reference)
        if existing is None:
            registry.copy(f"oci-archive:{archive}", reference)
            action = "created"
        elif existing == expected:
            action = "already present"
        else:
            raise RuntimeError(f"{reference} already holds {existing}; version tags are immutable")
        if registry.digest(reference) != expected:
            raise RuntimeError(f"{reference} does not resolve to the qualified digest")
        receipt.append({"reference": reference, "digest": expected, "action": action})
    if latest:
        for repository in repositories:
            reference = f"{repository}:latest"
            registry.copy(f"docker://{repository}@{expected}", reference)
            if registry.digest(reference) != expected:
                raise RuntimeError(f"{reference} does not resolve to the qualified digest")
            receipt.append({"reference": reference, "digest": expected, "action": "moved"})
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    inputs = commands.add_parser("inputs")
    inputs.add_argument("--directory", type=Path, required=True)
    inputs.add_argument("--output", type=Path, required=True)
    inputs.add_argument("--version", required=True, help="archive version, X.Y.Z")
    digests = commands.add_parser("digests")
    digests.add_argument("--archive", type=Path, required=True)
    check = commands.add_parser("verify")
    check.add_argument("--archive", type=Path, required=True)
    check.add_argument("--directory", type=Path, required=True, help="directory holding the Linux release archives")
    check.add_argument("--version", required=True, help="archive version, X.Y.Z")
    check.add_argument("--revision", required=True, help="the source commit the image labels name")
    check.add_argument("--created", required=True, help="the creation time the image labels name")
    push = commands.add_parser("publish")
    push.add_argument("--archive", type=Path, required=True)
    push.add_argument("--digest", required=True)
    push.add_argument("--tag", required=True)
    push.add_argument("--repository", action="append", required=True)
    push.add_argument("--latest", action="store_true")
    push.add_argument("--rehearsal-registry", metavar="HOST:PORT",
                      help="publish only to this throwaway plain-HTTP registry")
    args = parser.parse_args()
    if args.command == "inputs":
        print(json.dumps(stage_inputs(args.directory, args.output, args.version), indent=2, sort_keys=True))
    elif args.command == "digests":
        print(json.dumps(image_digests(args.archive), indent=2, sort_keys=True))
    elif args.command == "verify":
        identities = verify_image(args.archive, args.directory, args.version, args.revision, args.created)
        print(json.dumps(identities, indent=2, sort_keys=True))
    else:
        if not os.environ.get("REGISTRY_AUTH_FILE"):
            raise ValueError("REGISTRY_AUTH_FILE must name the registry login file")
        rehearsal = args.rehearsal_registry
        receipt = publish(args.archive, args.digest, args.repository, args.tag, args.latest,
                          Registry(tls_verify=rehearsal is None), rehearsal)
        print(json.dumps(receipt, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
