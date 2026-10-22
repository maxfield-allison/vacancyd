from __future__ import annotations

import importlib.util
from pathlib import Path
import tomllib
import unittest

SCRIPT = Path(__file__).with_name("qualify_container.py")
SPEC = importlib.util.spec_from_file_location("qualify_container", SCRIPT)
assert SPEC and SPEC.loader
qualify_container = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qualify_container)

DOCKERFILE = SCRIPT.parent.parent / "packaging" / "container" / "Dockerfile"


def good_inspect(version: str = "1.2.3") -> dict:
    return {"Config": {
        "User": "65532:65532",
        "Entrypoint": ["/usr/local/bin/vacancyd"],
        "Cmd": ["run", "--config", "/etc/vacancyd/vacancyd.toml"],
        "Healthcheck": {"Test": ["CMD", "/usr/local/bin/vacancyd", "healthcheck", "--config", "/etc/vacancyd/vacancyd.toml"]},
        "Env": ["PATH=/usr/bin", "STATE_DIRECTORY=/var/lib/vacancyd"],
        "Labels": {f"org.opencontainers.image.{key}": "x" for key in ("source", "licenses", "revision", "base.name")}
        | {"org.opencontainers.image.version": version},
    }}


class RenderConfigTests(unittest.TestCase):
    def test_headless_memory_configuration_requires_tokens_for_every_listener(self) -> None:
        config = tomllib.loads(qualify_container.render_config("upstream", "headless", "memory"))
        self.assertEqual(config["owner"]["presence"], "headless")
        self.assertFalse(config["auth"]["allow_unauthenticated_remote"])
        self.assertEqual({t["name"]: t["scopes"] for t in config["auth"]["token"]},
                         {"borrow": ["borrow"], "read": ["read"], "admin": ["read", "admin"]})
        self.assertEqual(config["upstream"][0]["target"], "http://upstream:18080")
        self.assertEqual(config["upstream"][0]["resource"], "memory")

    def test_gpu_configuration_lends_an_nvidia_resource(self) -> None:
        config = tomllib.loads(qualify_container.render_config("upstream", "auto", "gpu", "GPU-1234"))
        gpu = next(r for r in config["resource"] if r["id"] == "gpu0")
        self.assertEqual((gpu["kind"], gpu["selector"]), ("nvidia-gpu", "GPU-1234"))
        self.assertEqual(config["upstream"][0]["resource"], "gpu0")
        with self.assertRaises(ValueError):
            qualify_container.render_config("upstream", "auto", "disk")


class ImageMetadataTests(unittest.TestCase):
    def test_accepts_the_documented_contract(self) -> None:
        self.assertEqual(qualify_container.image_metadata_failures(good_inspect(), "1.2.3"), [])

    def test_reports_root_user_wrong_version_and_missing_health_check(self) -> None:
        inspect = good_inspect()
        inspect["Config"]["User"] = ""
        inspect["Config"]["Healthcheck"] = None
        failures = qualify_container.image_metadata_failures(inspect, "1.2.4")
        self.assertEqual(len(failures), 3, failures)

    def test_dockerfile_declares_what_the_qualification_expects(self) -> None:
        text = DOCKERFILE.read_text()
        self.assertIn("USER 65532:65532", text)
        self.assertIn('ENTRYPOINT ["/usr/local/bin/vacancyd"]', text)
        self.assertIn('CMD ["run", "--config", "/etc/vacancyd/vacancyd.toml"]', text)
        self.assertIn("STATE_DIRECTORY=/var/lib/vacancyd", text)
        # The admin socket's directory belongs to the service user.
        self.assertIn("chown 65532:65532 /out/var/lib/vacancyd /out/run/vacancyd", text)
        self.assertIn("chmod 0755 /out/run/vacancyd", text)
        for line in text.splitlines():
            if line.startswith("ARG ") and "_IMAGE=" in line:
                self.assertRegex(line, r"@sha256:[0-9a-f]{64}$", "base images are pinned by digest")


if __name__ == "__main__":
    unittest.main()
