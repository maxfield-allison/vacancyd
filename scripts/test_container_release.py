from __future__ import annotations

import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import urllib.error

SCRIPT = Path(__file__).with_name("container_release.py")
SPEC = importlib.util.spec_from_file_location("container_release", SCRIPT)
assert SPEC and SPEC.loader
container_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(container_release)

COMMIT = "a" * 40
REPOSITORY = "owner/vacancyd"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def release_directory(root: Path, version: str = "1.2.3") -> Path:
    directory = root / "dist"
    directory.mkdir()
    for target in container_release.ARCHES.values():
        archive = f"vacancyd-{version}-{target}.tar.gz"
        data = f"archive {target}".encode()
        (directory / archive).write_bytes(data)
        (directory / (archive + ".sha256")).write_bytes(f"{sha(data)}  {archive}\n".encode())
    # Other release files in the same directory are left behind.
    (directory / f"vacancyd-{version}-x86_64-pc-windows-msvc.zip").write_bytes(b"zip")
    return directory


def oci_archive(path: Path, platforms=("amd64", "arm64"), corrupt: bool = False) -> str:
    blobs: dict[str, bytes] = {}

    def add(data: bytes) -> str:
        digest = "sha256:" + sha(data)
        blobs[digest] = data
        return digest

    entries = []
    for arch in platforms:
        config = add(json.dumps({"architecture": arch, "os": "linux"}).encode())
        layer = add(f"layer {arch}".encode())
        manifest = json.dumps({"config": {"digest": config}, "layers": [{"digest": layer}]}).encode()
        entries.append({"digest": add(manifest), "platform": {"os": "linux", "architecture": arch}})
    index_digest = add(json.dumps({"manifests": entries}).encode())
    layout = json.dumps({"manifests": [{"mediaType": "application/vnd.oci.image.index.v1+json", "digest": index_digest}]})
    with tarfile.open(path, "w") as archive:
        members = {"index.json": layout.encode()}
        for digest, data in blobs.items():
            members["blobs/sha256/" + digest.split(":")[1]] = b"tampered" if corrupt and digest != index_digest and b"layer" in data else data
        for name, data in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return index_digest


REVISION = "c" * 40
CREATED = "2026-09-25T12:00:00-05:00"
LABELS = {"VERSION": "1.2.3", "REVISION": REVISION, "CREATED": CREATED}
BASE_ENV = ["PATH=/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/sbin:/bin",
            "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt"]


def tar_bytes(entries) -> bytes:
    """entries: (name, data or None for a directory, mode, owner[, type])."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name, data, mode, owner, *kind in entries:
            info = tarfile.TarInfo(name)
            info.mode, info.uid, info.gid = mode, owner, owner
            if kind:
                info.type, info.linkname = kind[0], "/usr/lib/libc.so.6"
                archive.addfile(info)
            elif data is None:
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
            else:
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def base_layers(architecture: str) -> list[bytes]:
    return [tar_bytes([("usr", None, 0o755, 0), ("usr/lib", None, 0o755, 0),
                       ("usr/lib/libc.so.6", f"glibc {architecture}".encode(), 0o755, 0)]),
            tar_bytes([("etc", None, 0o755, 0), ("etc/passwd", b"nonroot:x:65532:65532::/home/nonroot:/sbin/nologin\n", 0o644, 0)])]


def base_config(architecture: str) -> dict:
    return {"architecture": architecture, "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": ["sha256:" + sha(layer) for layer in base_layers(architecture)]},
            "config": {"Env": list(BASE_ENV), "User": "65532",
                       "Labels": {"org.opencontainers.image.vendor": "Base vendor", "dev.base.title": "base"}}}


def fake_base(reference: str, architecture: str) -> dict:
    assert reference == container_release.runtime_base(), reference
    return base_config(architecture)


def archive_files(architecture: str) -> dict[str, tuple[int, bytes]]:
    """A Linux release archive's files, {path under the archive's top directory: (mode, bytes)}."""
    return {"vacancyd": (0o755, f"ELF vacancyd {architecture}".encode()), "README.md": (0o644, b"readme"),
            "docs/container.md": (0o644, b"container guide"), "contrib/vacancyd.service": (0o644, b"[Unit]")}


def release_archives(directory: Path, version: str = "1.2.3", files=archive_files) -> None:
    """The two Linux release archives laid out as scripts/package_release.py does: files only."""
    directory.mkdir(parents=True, exist_ok=True)
    for architecture, target in container_release.ARCHES.items():
        stem = f"vacancyd-{version}-{target}"
        entries = [(f"{stem}/{name}", data, mode, 0) for name, (mode, data) in files(architecture).items()]
        path = directory / f"{stem}.tar.gz"
        path.write_bytes(gzip.compress(tar_bytes(entries), mtime=0))
        (directory / f"{stem}.tar.gz.sha256").write_text(f"{sha(path.read_bytes())}  {path.name}\n")


def payload_layer(files: dict[str, tuple[int, bytes]]) -> list:
    """What the Dockerfile's payload stage copies onto the base, written out by hand."""
    service = 65532
    documents = {"usr/share/doc/vacancyd/" + name: (mode, data) for name, (mode, data) in files.items() if name != "vacancyd"}
    parents = sorted({str(Path(name).parent) for name in documents} - {"usr/share/doc/vacancyd"})
    entries = [(name, None, 0o755, 0) for name in ("etc", "etc/vacancyd", "run")]
    entries += [("run/vacancyd", None, 0o755, service)]
    entries += [(name, None, 0o755, 0) for name in ("usr", "usr/local", "usr/local/bin", "usr/share", "usr/share/doc",
                                                    "usr/share/doc/vacancyd", *parents, "var", "var/lib")]
    entries += [("var/lib/vacancyd", None, 0o700, service)]
    entries += [("usr/local/bin/vacancyd", files["vacancyd"][1], 0o755, 0)]
    entries += [(name, data, 0o755 if mode & 0o111 else 0o644, 0) for name, (mode, data) in sorted(documents.items())]
    return entries


def image_config(architecture: str, layers: list[bytes], labels: dict[str, str]) -> dict:
    labels = container_release.dockerfile_labels({**labels, "RUNTIME_IMAGE": container_release.runtime_base()})
    base = base_config(architecture)["config"]
    return {"architecture": architecture, "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": ["sha256:" + sha(layer) for layer in layers]},
            "config": {"User": container_release.USER, "Entrypoint": list(container_release.ENTRYPOINT),
                       "Cmd": list(container_release.CMD), "Env": BASE_ENV + container_release.ENV,
                       "ExposedPorts": {"9811/tcp": {}}, "Labels": {**base["Labels"], **labels},
                       "Healthcheck": {"Test": list(container_release.HEALTHCHECK), "Retries": 3}}}


def release_image(path: Path, change=None, files=archive_files, labels=None) -> dict:
    """A two-platform OCI archive built the way the Dockerfile builds it.

    change(architecture, entries, layers, config) may tamper before the image
    is written: entries is the payload layer's entry list, layers the list of
    uncompressed layer tars (payload last, filled in after the call when left
    as None), config a dict whose "config" updates the image configuration and
    whose "diff_ids" replaces the recorded layer IDs.
    """
    labels = labels or LABELS
    blobs: dict[str, bytes] = {}

    def add(data: bytes) -> str:
        digest = "sha256:" + sha(data)
        blobs[digest] = data
        return digest

    manifests, configs, identities = [], {}, {}
    for architecture in container_release.ARCHES:
        entries = payload_layer(files(architecture))
        layers = base_layers(architecture) + [None]
        config: dict = {}
        if change:
            change(architecture, entries, layers, config)
        if layers[-1] is None:
            layers[-1] = tar_bytes(entries)
        built = image_config(architecture, layers, labels)
        built["config"].update(config.get("config", {}))
        if "diff_ids" in config:
            built["rootfs"]["diff_ids"] = config["diff_ids"]
        config_digest = add(json.dumps(built).encode())
        configs[architecture] = config_digest
        identities[architecture] = built["rootfs"]["diff_ids"]
        manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest},
                    "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                                "digest": add(gzip.compress(layer, mtime=0))} for layer in layers]}
        manifests.append({"mediaType": manifest["mediaType"], "digest": add(json.dumps(manifest).encode()),
                          "platform": {"os": "linux", "architecture": architecture}})
    index = add(json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
                            "manifests": manifests}).encode())
    with tarfile.open(path, "w") as archive:
        members = {"index.json": json.dumps({"manifests": [{"mediaType": "application/vnd.oci.image.index.v1+json",
                                                            "digest": index}]}).encode()}
        members.update({"blobs/sha256/" + digest.split(":")[1]: data for digest, data in blobs.items()})
        for name, data in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return {"index": index, "configs": configs, "diff_ids": identities}


class FakeRegistry:
    def __init__(self, existing=None):
        self.tags = dict(existing or {})
        self.copies = []
        self.index = None

    def digest(self, reference):
        return self.tags.get(reference)

    def copy(self, source, reference):
        self.copies.append((source, reference))
        self.tags[reference] = self.index


class StageInputsTests(unittest.TestCase):
    def test_copies_exactly_the_two_linux_archives_and_reports_their_digests(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = release_directory(root)
            staged = container_release.stage_inputs(directory, root / "context", "1.2.3")
            self.assertEqual(staged, {
                f"vacancyd-1.2.3-{target}.tar.gz": sha(f"archive {target}".encode())
                for target in container_release.ARCHES.values()})
            self.assertEqual(sorted(p.name for p in (root / "context").iterdir()),
                             sorted([*staged, *(name + ".sha256" for name in staged)]))

    def test_refuses_an_archive_that_differs_from_its_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = release_directory(root)
            (directory / "vacancyd-1.2.3-aarch64-unknown-linux-gnu.tar.gz").write_bytes(b"substituted")
            with self.assertRaisesRegex(ValueError, "does not match its .sha256 sidecar"):
                container_release.stage_inputs(directory, root / "context", "1.2.3")
            self.assertFalse((root / "context").exists())

    def test_refuses_a_missing_archive_or_sidecar_and_a_tag_for_a_version(self) -> None:
        for missing in ("vacancyd-1.2.3-x86_64-unknown-linux-gnu.tar.gz",
                        "vacancyd-1.2.3-aarch64-unknown-linux-gnu.tar.gz.sha256"):
            with self.subTest(missing), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                directory = release_directory(root)
                (directory / missing).unlink()
                with self.assertRaisesRegex(ValueError, "missing release file"):
                    container_release.stage_inputs(directory, root / "context", "1.2.3")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = release_directory(root)
            with self.assertRaisesRegex(ValueError, "without v"):
                container_release.stage_inputs(directory, root / "context", "v1.2.3")

    def test_refuses_an_existing_context(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = release_directory(root)
            (root / "context").mkdir()
            with self.assertRaisesRegex(ValueError, "must be new"):
                container_release.stage_inputs(directory, root / "context", "1.2.3")


class ImageDigestTests(unittest.TestCase):
    def test_reads_index_and_platform_digests(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            digests = container_release.image_digests(path)
            self.assertEqual(digests["index"], index)
            self.assertEqual(set(digests["platforms"]), {"linux/amd64", "linux/arm64"})

    def test_refuses_a_missing_platform_or_corrupt_blob(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            oci_archive(path, platforms=("amd64",))
            with self.assertRaisesRegex(ValueError, "expected"):
                container_release.image_digests(path)
            oci_archive(path, corrupt=True)
            with self.assertRaisesRegex(ValueError, "corrupt"):
                container_release.image_digests(path)


class VerifyImageTests(unittest.TestCase):
    """The image must be the pinned base plus exactly the release archive, by content."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        release_archives(self.root / "dist")
        self.image = self.root / "image.oci.tar"

    def verify(self, **overrides):
        values = {"version": "1.2.3", "revision": REVISION, "created": CREATED, **overrides}
        return container_release.verify_image(self.image, self.root / "dist", values["version"], values["revision"],
                                              values["created"], base_config=fake_base)

    def refused(self, change, message: str) -> None:
        release_image(self.image, change)
        with self.assertRaisesRegex(ValueError, message):
            self.verify()

    def test_the_genuine_image_is_identified_per_platform(self) -> None:
        built = release_image(self.image)
        identities = self.verify()
        self.assertEqual(set(identities), {"linux/amd64", "linux/arm64"})
        for architecture in container_release.ARCHES:
            identity = identities[f"linux/{architecture}"]
            self.assertEqual(identity["config"], built["configs"][architecture])
            self.assertEqual(len(identity["diff_ids"]), 3)

    def test_a_changed_binary_or_document_is_refused(self) -> None:
        def binary(architecture, entries, layers, config):
            if architecture == "arm64":
                entries[:] = [(n, b"ELF implant" if n == "usr/local/bin/vacancyd" else d, m, o) for n, d, m, o in entries]
        self.refused(binary, "linux/arm64: usr/local/bin/vacancyd differs from the release archive")

        def document(architecture, entries, layers, config):
            entries[:] = [(n, b"other" if n.endswith("README.md") else d, m, o) for n, d, m, o in entries]
        self.refused(document, "README.md differs")

    def test_extra_missing_deleting_or_linked_payload_entries_are_refused(self) -> None:
        cases = {
            "unexpected file usr/lib/preload.so": lambda e: e.append(("usr/lib/preload.so", b"x", 0o755, 0)),
            "usr/share/doc/vacancyd/README.md is missing": lambda e: e.remove(next(x for x in e if x[0].endswith("README.md"))),
            "deletes a base image file": lambda e: e.append(("etc/.wh.passwd", b"", 0o644, 0)),
            "is not a regular file or directory": lambda e: e.append(("usr/local/bin/sh", None, 0o777, 0, tarfile.SYMTYPE)),
            "unexpected directory opt": lambda e: e.append(("opt", None, 0o755, 0)),
        }
        for message, edit in cases.items():
            with self.subTest(message):
                self.refused(lambda architecture, entries, layers, config: edit(entries), message)

    def test_ownership_and_modes_are_the_documented_ones(self) -> None:
        def loosen(architecture, entries, layers, config):
            entries[:] = [(n, d, 0o777 if n == "var/lib/vacancyd" else m, o) for n, d, m, o in entries]
        self.refused(loosen, "var/lib/vacancyd has owner 65532:65532 mode 777")

        def owner(architecture, entries, layers, config):
            entries[:] = [(n, d, m, 65532 if n == "usr/local/bin/vacancyd" else o) for n, d, m, o in entries]
        self.refused(owner, "usr/local/bin/vacancyd has owner 65532:65532")

    def test_the_layers_below_the_payload_must_be_the_pinned_base(self) -> None:
        def replaced(architecture, entries, layers, config):
            layers[0] = tar_bytes([("usr", None, 0o755, 0), ("usr/lib/libc.so.6", b"trojan", 0o755, 0)])
        self.refused(replaced, "layers below the payload are not")

        def extra_layer(architecture, entries, layers, config):
            layers.insert(-1, tar_bytes([("tmp", None, 0o755, 0)]))
        self.refused(extra_layer, "layers below the payload are not")

    def test_a_configuration_that_misstates_its_layers_is_refused(self) -> None:
        def lying(architecture, entries, layers, config):
            layers[-1] = tar_bytes(entries + [("usr/lib/implant.so", b"x", 0o755, 0)])
            honest = tar_bytes(entries)
            config["diff_ids"] = ["sha256:" + sha(layer) for layer in base_layers(architecture)] + ["sha256:" + sha(honest)]
        self.refused(lying, "layer 2 does not match its diff ID")

    def test_the_configuration_is_the_dockerfile_s(self) -> None:
        cases = {
            "Entrypoint is": {"Entrypoint": ["/usr/lib/libc.so.6"]},
            "Env is": {"Env": BASE_ENV + container_release.ENV + ["LD_PRELOAD=/usr/lib/libc.so.6"]},
            "User is": {"User": "0"},
            "Cmd is": {"Cmd": ["run"]},
            "health check": {"Healthcheck": {"Test": ["NONE"]}},
            "exposed ports": {"ExposedPorts": {"9811/tcp": {}, "22/tcp": {}}},
            "WorkingDir differs": {"WorkingDir": "/tmp"},
        }
        for message, setting in cases.items():
            with self.subTest(message):
                self.refused(lambda architecture, entries, layers, config: config.update({"config": setting}), message)

    def test_labels_name_this_version_revision_date_and_base(self) -> None:
        release_image(self.image)
        for override in ({"revision": "d" * 40}, {"created": "2026-09-26T00:00:00Z"}, {"version": "1.2.4"}):
            with self.subTest(override), self.assertRaises(ValueError):
                self.verify(**override)

    def test_the_base_reference_is_read_from_the_dockerfile_and_must_be_pinned(self) -> None:
        self.assertRegex(container_release.runtime_base(), r"@sha256:[0-9a-f]{64}$")
        dockerfile = self.root / "Dockerfile"
        for text in ("ARG RUNTIME_IMAGE=cgr.dev/chainguard/glibc-dynamic:latest\n",
                     "ARG RUNTIME_IMAGE=a.example/x@sha256:" + "0" * 64 + "\nARG RUNTIME_IMAGE=b.example/y@sha256:" + "1" * 64 + "\n"):
            dockerfile.write_text(text)
            with self.assertRaisesRegex(ValueError, "pin exactly one"):
                container_release.runtime_base(dockerfile)

    def test_the_declared_configuration_matches_the_dockerfile(self) -> None:
        text = container_release.DOCKERFILE.read_text(encoding="utf-8")
        self.assertIn(f"USER {container_release.USER}", text)
        self.assertIn(f"ENTRYPOINT {json.dumps(container_release.ENTRYPOINT)}", text)
        self.assertIn(f"CMD {json.dumps(container_release.CMD)}", text)
        self.assertIn(f"CMD {json.dumps(container_release.HEALTHCHECK[1:])}", text)
        self.assertIn("EXPOSE 9811", text)
        for setting in container_release.ENV:
            self.assertIn(setting, text)
        directories = container_release.PAYLOAD_DIRECTORIES
        leaves = [name for name in directories if not any(other.startswith(name + "/") for other in directories)]
        for leaf in leaves:
            self.assertIn("/out/" + leaf, text)
        service = sorted(name for name, (owner, _) in directories.items() if owner)
        self.assertIn("chown 65532:65532 " + " ".join("/out/" + name for name in reversed(service)), text)
        self.assertIn("chmod 0700 /out/var/lib/vacancyd", text)
        self.assertEqual(directories["var/lib/vacancyd"], (65532, 0o700))


class FakeResponse(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class FakeOpener:
    """Serves a registry that wants an anonymous bearer token, and records every request."""

    def __init__(self, documents: dict[str, bytes], realm: str = "https://registry.example/token"):
        self.documents, self.realm, self.requests = documents, realm, []

    def open(self, request, timeout=None):
        url = request.full_url if hasattr(request, "full_url") else request
        headers = dict(request.header_items()) if hasattr(request, "header_items") else {}
        self.requests.append((url, headers))
        if url.startswith(self.realm):
            return FakeResponse(json.dumps({"token": "anonymous"}).encode())
        if headers.get("Authorization") != "Bearer anonymous":
            raise urllib.error.HTTPError(url, 401, "Unauthorized", {
                "WWW-Authenticate": f'Bearer realm="{self.realm}",service="registry.example",scope="repository:x:pull"'}, None)
        return FakeResponse(self.documents[url.rsplit("/", 1)[1]])


class BaseImageReadTests(unittest.TestCase):
    def reader(self, documents, **options):
        reader = container_release.BaseImages()
        reader.opener = FakeOpener(documents, **options)
        return reader

    def test_reads_the_platform_configuration_by_digest_with_an_anonymous_token(self) -> None:
        config = json.dumps(base_config("arm64")).encode()
        manifest = json.dumps({"config": {"digest": "sha256:" + sha(config)}, "layers": []}).encode()
        index = json.dumps({"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"digest": "sha256:" + sha(manifest), "platform": {"os": "linux", "architecture": "arm64"}}]}).encode()
        documents = {"sha256:" + sha(data): data for data in (config, manifest, index)}
        reader = self.reader(documents)
        reference = "registry.example/base/image:latest@sha256:" + sha(index)
        self.assertEqual(reader.config(reference, "arm64"), base_config("arm64"))
        self.assertTrue(reader.opener.requests[0][0].startswith("https://registry.example/v2/base/image/manifests/"))
        with self.assertRaisesRegex(ValueError, "no single linux/amd64"):
            reader.config(reference, "amd64")

    def test_resolves_a_tag_anonymously_to_the_digest_of_the_manifest_it_names(self) -> None:
        index = json.dumps({"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": []}).encode()
        reader = self.reader({"v1.0.0": index})
        self.assertEqual(reader.resolve("registry.example", "owner/vacancyd", "v1.0.0"), "sha256:" + sha(index))
        self.assertEqual(reader.opener.requests[-1][0], "https://registry.example/v2/owner/vacancyd/manifests/v1.0.0")
        with self.assertRaisesRegex(ValueError, "invalid tag"):
            reader.resolve("registry.example", "owner/vacancyd", "../blobs/x")

    def test_refuses_content_that_is_not_the_requested_digest(self) -> None:
        wanted = "sha256:" + sha(b"expected")
        reader = self.reader({wanted: b"substituted"})
        with self.assertRaisesRegex(ValueError, "served other content"):
            reader.read("registry.example", "base/image", "manifests", wanted)

    def test_refuses_an_unpinned_reference_or_a_plain_http_token_service(self) -> None:
        with self.assertRaisesRegex(ValueError, "not pinned"):
            self.reader({}).config("registry.example/base/image:latest", "amd64")
        reader = self.reader({}, realm="http://registry.example/token")
        with self.assertRaisesRegex(RuntimeError, "not HTTPS"):
            reader.read("registry.example", "base/image", "manifests", "sha256:" + "0" * 64)

    def test_the_registry_token_is_not_forwarded_to_redirected_storage(self) -> None:
        import urllib.request
        handler = container_release._DropAuthorizationOnRedirect()
        request = urllib.request.Request("https://registry.example/v2/x/blobs/sha256:0",
                                         headers={"Authorization": "Bearer anonymous"})
        same = handler.redirect_request(request, None, 307, "Redirect", {}, "https://registry.example/other")
        other = handler.redirect_request(request, None, 307, "Redirect", {}, "https://storage.example/blob?sig=1")
        self.assertTrue(same.has_header("Authorization"))
        self.assertFalse(other.has_header("Authorization"))


class PublishTests(unittest.TestCase):
    repositories = ["docker.io/example/vacancyd", "ghcr.io/owner-name/vacancyd"]

    def test_creates_version_tags_then_moves_latest_to_the_same_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            registry = FakeRegistry()
            registry.index = index
            receipt = container_release.publish(path, index, self.repositories, "v1.2.3", True, registry)
            self.assertEqual([item["reference"] for item in receipt], [
                "docker.io/example/vacancyd:v1.2.3", "ghcr.io/owner-name/vacancyd:v1.2.3",
                "docker.io/example/vacancyd:latest", "ghcr.io/owner-name/vacancyd:latest"])
            self.assertTrue(all(item["digest"] == index for item in receipt))
            self.assertEqual(registry.copies[0][0], f"oci-archive:{path}")
            self.assertEqual(registry.copies[2][0], f"docker://docker.io/example/vacancyd@{index}")

    def test_a_rerun_accepts_identical_content_and_leaves_latest_alone(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            registry = FakeRegistry({f"{r}:v1.2.3": index for r in self.repositories})
            registry.index = index
            receipt = container_release.publish(path, index, self.repositories, "v1.2.3", False, registry)
            self.assertEqual({item["action"] for item in receipt}, {"already present"})
            self.assertEqual(registry.copies, [])

    def test_never_overwrites_a_version_tag_with_other_content(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            registry = FakeRegistry({"docker.io/example/vacancyd:v1.2.3": "sha256:" + "0" * 64})
            registry.index = index
            with self.assertRaisesRegex(RuntimeError, "immutable"):
                container_release.publish(path, index, self.repositories, "v1.2.3", True, registry)
            self.assertEqual(registry.copies, [])

    def test_refuses_an_unqualified_archive_or_unexpected_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            registry = FakeRegistry()
            with self.assertRaisesRegex(ValueError, "not the qualified image"):
                container_release.publish(path, "sha256:" + "1" * 64, self.repositories, "v1.2.3", False, registry)
            with self.assertRaisesRegex(ValueError, "unexpected repository"):
                container_release.publish(path, index, ["quay.io/example/vacancyd"], "v1.2.3", False, registry)
            with self.assertRaisesRegex(ValueError, "stable"):
                container_release.publish(path, index, self.repositories, "v1.2.3-rc.1", False, registry)
            self.assertEqual(registry.copies, [])


class RehearsalRegistryTests(unittest.TestCase):
    def test_a_rehearsal_writes_only_to_its_throwaway_registry(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.tar"
            index = oci_archive(path)
            registry = FakeRegistry()
            registry.index = index
            repositories = ["registry:5000/example/vacancyd", "registry:5000/owner-name/vacancyd"]
            receipt = container_release.publish(path, index, repositories, "v1.2.3", True, registry,
                                                rehearsal_host="registry:5000")
            self.assertEqual(len(receipt), 4)
            for public in ("docker.io/example/vacancyd", "ghcr.io/owner-name/vacancyd",
                           "registry:5001/example/vacancyd", "registry:5000/example/other"):
                with self.subTest(public), self.assertRaisesRegex(ValueError, "unexpected repository"):
                    container_release.publish(path, index, [public], "v1.2.3", False, registry,
                                              rehearsal_host="registry:5000")
            with self.assertRaisesRegex(ValueError, "unexpected repository"):
                container_release.publish(path, index, repositories, "v1.2.3", False, registry)
            with self.assertRaisesRegex(ValueError, "host:port"):
                container_release.publish(path, index, repositories, "v1.2.3", False, registry,
                                          rehearsal_host="https://registry")

    def test_the_rehearsal_client_turns_off_tls_and_the_release_client_never_does(self) -> None:
        self.assertEqual(container_release.Registry().dest_tls, [])
        self.assertEqual(container_release.Registry().tls, [])
        self.assertEqual(container_release.Registry(tls_verify=False).dest_tls, ["--dest-tls-verify=false"])


if __name__ == "__main__":
    unittest.main()
