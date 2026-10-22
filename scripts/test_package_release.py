from __future__ import annotations

import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import xml.etree.ElementTree as ET
import zipfile


SCRIPT = Path(__file__).with_name("package_release.py")
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT)
assert SPEC and SPEC.loader
package_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_release)


class PackageReleaseTests(unittest.TestCase):
    def test_msi_includes_every_document_and_asset(self) -> None:
        repository = SCRIPT.parent.parent
        package = ET.parse(repository / "wix" / "Package.wxs")
        prefix = "$(var.RepositoryRoot)\\"
        included = {
            element.attrib["Source"][len(prefix):].replace("\\", "/")
            for element in package.iter("{http://wixtoolset.org/schemas/v4/wxs}File")
            if element.attrib.get("Source", "").startswith(prefix)
        }
        expected = {
            path.relative_to(repository).as_posix()
            for path in (repository / "docs").rglob("*")
            if path.is_file()
        }
        self.assertFalse(expected - included, f"MSI omits documentation: {sorted(expected - included)}")
        self.assertTrue({"THIRD_PARTY_LICENSES", "SBOM.cdx.json", "THIRD_PARTY_SOURCES.tar.gz"}.issubset(included))
        self.assertTrue({"LICENSE-APACHE", "NOTICE"}.issubset(included))
        self.assertNotIn("LICENSE-MIT", included)

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "vacancyd"
        self.binary.write_bytes(b"release-binary")
        self.session_binary = self.binary.with_name("vacancyd-session.exe")
        self.session_binary.write_bytes(b"background-agent-binary")
        self.binary.with_name("vacancyd-console.exe").write_bytes(b"read-only-console-binary")

    def run_packager(self, target: str, destination: Path) -> Path:
        environment = os.environ.copy()
        environment["SOURCE_DATE_EPOCH"] = "1700000000"
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--target",
                target,
                "--binary",
                str(self.binary),
                "--output-dir",
                str(destination),
                "--expected-version",
                f"v{package_release.project_version()}",
            ],
            check=True,
            capture_output=True,
            text=True,
            env=environment,
        )
        return Path(result.stdout.splitlines()[0])

    def test_linux_archive_is_reproducible_and_has_service_unit(self) -> None:
        first = self.run_packager("x86_64-unknown-linux-gnu", self.root / "first")
        second = self.run_packager("x86_64-unknown-linux-gnu", self.root / "second")
        self.assertEqual(hashlib.sha256(first.read_bytes()).digest(), hashlib.sha256(second.read_bytes()).digest())
        with tarfile.open(first, "r:gz") as archive:
            names = archive.getnames()
            license_name = next(name for name in names if name.endswith("/LICENSE-APACHE"))
            self.assertEqual(archive.extractfile(license_name).read(),
                             (SCRIPT.parent.parent / "LICENSE-APACHE").read_bytes())
        self.assertFalse(any(name.endswith("/LICENSE-MIT") for name in names))
        self.assertTrue(any(name.endswith("/vacancyd") for name in names))
        self.assertTrue(any(name.endswith("/contrib/vacancyd.service") for name in names))
        for notice in ("THIRD_PARTY_LICENSES", "SBOM.cdx.json", "THIRD_PARTY_SOURCES.tar.gz", "NOTICE"):
            self.assertTrue(any(name.endswith("/" + notice) for name in names))
        self.assertTrue(any(name.endswith("/SECURITY.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/architecture.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/authentication.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/calibration.md") for name in names))

    def test_windows_archive_is_reproducible_and_has_service_guide(self) -> None:
        first = self.run_packager("x86_64-pc-windows-msvc", self.root / "first")
        second = self.run_packager("x86_64-pc-windows-msvc", self.root / "second")
        self.assertEqual(hashlib.sha256(first.read_bytes()).digest(), hashlib.sha256(second.read_bytes()).digest())
        with zipfile.ZipFile(first) as archive:
            names = archive.namelist()
            license_name = next(name for name in names if name.endswith("/LICENSE-APACHE"))
            self.assertEqual(archive.read(license_name),
                             (SCRIPT.parent.parent / "LICENSE-APACHE").read_bytes())
        self.assertFalse(any(name.endswith("/LICENSE-MIT") for name in names))
        self.assertTrue(any(name.endswith("/vacancyd.exe") for name in names))
        self.assertTrue(any(name.endswith("/vacancyd-session.exe") for name in names))
        self.assertTrue(any(name.endswith("/vacancyd-console.exe") for name in names))
        for notice in ("THIRD_PARTY_LICENSES", "SBOM.cdx.json", "THIRD_PARTY_SOURCES.tar.gz", "NOTICE"):
            self.assertTrue(any(name.endswith("/" + notice) for name in names))
        self.assertTrue(any(name.endswith("/docs/windows-service.md") for name in names))
        self.assertTrue(any(name.endswith("/SECURITY.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/architecture.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/authentication.md") for name in names))
        self.assertTrue(any(name.endswith("/docs/calibration.md") for name in names))

    def test_version_mismatch_fails(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--target",
                "x86_64-unknown-linux-gnu",
                "--binary",
                str(self.binary),
                "--output-dir",
                str(self.root / "mismatch"),
                "--expected-version",
                "v9.9.9",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release version mismatch", result.stderr)

    def test_windows_payload_requires_the_background_agent(self) -> None:
        self.session_binary.unlink()
        with self.assertRaisesRegex(FileNotFoundError, "vacancyd-session.exe"):
            package_release.payload(self.binary, "x86_64-pc-windows-msvc")

    def test_msi_owns_the_session_binary_and_menu_shortcuts(self) -> None:
        package = ET.parse(SCRIPT.parent.parent / "wix" / "Package.wxs")
        ns = "{http://wixtoolset.org/schemas/v4/wxs}"
        files = {element.attrib["Id"] for element in package.iter(ns + "File")}
        self.assertIn("SessionAgentExe", files)
        self.assertIn("OwnerConsoleExe", files)
        shortcuts = {element.attrib["Id"] for element in package.iter(ns + "Shortcut")}
        self.assertIn("SetupShortcut", shortcuts)
        self.assertIn("SessionAgentShortcut", shortcuts)
        self.assertIn("SessionAgentStartup", shortcuts)
        self.assertIn("OwnerConsoleShortcut", shortcuts)
        self.assertTrue(any(element.attrib.get("Id") == "RemoveStartMenuDirectory"
                            for element in package.iter(ns + "RemoveFolder")))

    def test_msi_owns_logon_and_transactional_upgrade(self) -> None:
        package = ET.parse(SCRIPT.parent.parent / "wix" / "Package.wxs")
        ns = "{http://wixtoolset.org/schemas/v4/wxs}"
        self.assertEqual(package.find(ns + "Package").attrib["Scope"], "perMachine")
        upgrade = next(package.iter(ns + "MajorUpgrade"))
        self.assertEqual(upgrade.attrib["Schedule"], "afterInstallExecute")
        # A same-version rebuild (signed over unsigned) replaces the old
        # product instead of installing beside it.
        self.assertEqual(upgrade.attrib["AllowSameVersionUpgrades"], "yes")
        self.assertNotIn("AllowDowngrades", upgrade.attrib)
        # It also overwrites equal-version files, such as the versioned console.
        properties = {node.attrib["Id"]: node.attrib.get("Value")
                      for node in package.iter(ns + "Property")}
        self.assertEqual(properties.get("REINSTALLMODE"), "emus")
        sequence = package.find(ns + "Package").find(ns + "InstallExecuteSequence")
        conditions = {node.attrib["Action"]: node.attrib.get("Condition")
                      for node in sequence.findall(ns + "Custom")}
        for action in ("override Wix4SchedServiceConfig_X64",
                       "SetReportedFailureRecovery", "ReportedFailureRecovery"):
            self.assertEqual(conditions[action],
                             "NOT Installed AND NOT WIX_UPGRADE_DETECTED AND $ServiceExecutable = 3")
        agent = next(file for file in package.iter(ns + "File")
                     if file.attrib["Id"] == "SessionAgentExe")
        startup = next(shortcut for shortcut in agent.iter(ns + "Shortcut")
                       if shortcut.attrib["Id"] == "SessionAgentStartup")
        self.assertEqual(startup.attrib["Directory"], "StartupFolder")
        self.assertEqual(startup.attrib["Advertise"], "yes")
        self.assertNotIn("Target", startup.attrib)  # installed file, no shell launcher
        # Session lifecycle still uses MSI-owned shortcuts, not custom launch
        # or kill commands. Privileged actions have fixed installed executables
        # and arguments for service recovery and configuration protection.
        actions = {action.attrib["Id"]: action.attrib
                   for action in package.iter(ns + "CustomAction")}
        self.assertEqual(set(actions), {"SetReportedFailureRecovery", "ReportedFailureRecovery",
                                        "SetProtectConfiguration", "ProtectConfiguration"})
        self.assertEqual(actions["SetReportedFailureRecovery"]["Property"], "ReportedFailureRecovery")
        self.assertEqual(actions["SetReportedFailureRecovery"]["Value"],
                         '"[System64Folder]sc.exe" failureflag vacancyd 1')
        self.assertEqual(actions["ReportedFailureRecovery"]["DllEntry"], "WixQuietExec")
        self.assertEqual(actions["ReportedFailureRecovery"]["Execute"], "deferred")
        self.assertEqual(actions["ReportedFailureRecovery"]["Impersonate"], "no")
        self.assertEqual(actions["ReportedFailureRecovery"]["Return"], "check")
        self.assertEqual(actions["SetProtectConfiguration"]["Value"],
                         '\"[#VacancydExe]\" protect-config --config \"[CommonAppDataVacancyd]vacancyd.toml\" --require-admin-owner')
        for key, value in {"DllEntry": "WixQuietExec", "Execute": "deferred", "Impersonate": "no", "Return": "check"}.items():
            self.assertEqual(actions["ProtectConfiguration"][key], value)
        permissions = {node.attrib["Sddl"] for node in package.iter(ns + "PermissionEx")}
        self.assertEqual(permissions, {"O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)", "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)"})
        # Keep machine-owned feature targets in the database, while requiring
        # Windows Installer to generate ordinary links rather than self-repair
        # activation that cannot inspect the admin-only configuration key path.
        self.assertEqual(properties['DISABLEADVTSHORTCUTS'], '1')
        for shortcut in package.iter(ns + "Shortcut"):
            self.assertEqual(shortcut.attrib["Advertise"], "yes")


if __name__ == "__main__":
    unittest.main()
