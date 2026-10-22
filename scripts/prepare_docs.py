#!/usr/bin/env python3
"""Stage the public guides, preserving working links on the documentation host."""

from __future__ import annotations

from pathlib import Path
import json
import posixpath
import re
import shutil
from urllib.parse import unquote, urlsplit, urlunsplit

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "build" / "docs-site"
REPOSITORY = "https://github.com/maxfield-allison/vacancyd"
LINK = re.compile(r"(?P<start>!?\[[^\]\n]*\]\()(?P<url>[^\s)]+)(?P<end>\))")

DESCRIPTIONS = {
    "index.md": "Keep your workstation. Let your homelab borrow its spare capacity for local AI. Setup, house rules and platform support for Windows and Linux.",
    "setup.md": "Install and configure vacancyd, verify a protected configuration and activate resource lending.",
    "windows-service.md": "Install and manage the vacancyd Windows service, owner console and session agent.",
    "container.md": "Run vacancyd in a Linux container with explicit presence policy, GPU access and verified images.",
    "authentication.md": "Configure scoped borrower, reader and administrator access, local administration and trusted listeners.",
    "compatibility.md": "Understand vacancyd version compatibility, configuration upgrades and session-reporter matching.",
    "console.md": "Use the owner console to inspect lending, resource checks, workload decisions and diagnostics.",
    "console-controls.md": "Understand the owner console's lending controls and local administrator requirements.",
    "console-settings.md": "Configure supported resource, workload and owner policies from the vacancyd console.",
    "split-agent.md": "How the Windows session agent supplies desktop-presence signals to the background service.",
    "agent-startup.md": "Start and diagnose the Windows session agent, including logon startup and service upgrades.",
    "admission.md": "How vacancyd admits, refuses and reclaims borrowed work, including cancel, drain and finish policies.",
    "tiers.md": "Understand and measure the resource-availability tiers exposed by vacancyd.",
    "calibration.md": "Calibrate GPU pressure policy and understand its measurement and platform limits.",
    "gpu-telemetry-metrics.md": "Interpret vacancyd GPU telemetry and Prometheus resource metrics.",
    "diagnostics.md": "Capture bounded debug logs and export redacted vacancyd diagnostics for investigation.",
    "litellm-503-interop.md": "Configure borrower behavior around vacancyd HTTP 503 refusals and LiteLLM failover.",
    "architecture.md": "Explore vacancyd's resource probes, presence backends, proxy adapters and admission boundaries.",
    "prior-art.md": "Compare resource admission with related approaches to inference routing and model lifetime.",
    "dependency-policy.md": "Review vacancyd's dependency inventory, licenses, vendored modifications and source notices.",
    "releasing.md": "Verify vacancyd release downloads, signed checksums, source tags and container attestations.",
    "security.md": "Report a vacancyd vulnerability privately and understand the supported security-reporting process.",
    "changelog.md": "Read the changes and release history for vacancyd.",
    "brand.md": "The vacancyd vacancy-sign identity: amber and slate states, typography, marks and product language.",
}


def rewrite_links(text: str, source: str, destinations: dict[str, str]) -> str:
    def replace(match: re.Match[str]) -> str:
        raw = match["url"]
        url = urlsplit(raw)
        if url.scheme or url.netloc or not url.path or url.path.startswith("/"):
            return match[0]
        original = posixpath.normpath(
            posixpath.join(posixpath.dirname(source), unquote(url.path))
        )
        if original in destinations:
            parent = posixpath.dirname(destinations[source]) or "."
            path = posixpath.relpath(destinations[original], parent)
        else:
            target = ROOT / original
            if not target.exists() or not target.resolve().is_relative_to(ROOT):
                raise ValueError(
                    f"{source}: relative link has no public source: {original}"
                )
            kind = "tree" if target.is_dir() else "blob"
            path = f"{REPOSITORY}/{kind}/main/{original}"
        return (
            match["start"]
            + urlunsplit(("", "", path, url.query, url.fragment))
            + match["end"]
        )

    return LINK.sub(replace, text)


def main() -> None:
    sources = {
        "README.md": "index.md",
        "SECURITY.md": "security.md",
        "CHANGELOG.md": "changelog.md",
    }
    sources.update(
        {
            p.relative_to(ROOT).as_posix(): p.relative_to(ROOT / "docs").as_posix()
            for p in (ROOT / "docs").rglob("*")
            if p.is_file()
        }
    )
    sources["docs-theme/vacancyd.css"] = "stylesheets/vacancyd.css"
    sources["docs-theme/keyboard.js"] = "javascripts/keyboard.js"
    sources.update(
        {
            f"console/web/assets/fonts/{name}": f"assets/fonts/{name}"
            for name in (
                "Archivo.ttf",
                "Archivo-OFL.txt",
                "IBMPlexMono.ttf",
                "IBMPlexMono-OFL.txt",
            )
        }
    )
    # Only this script's generated staging tree is replaced; source guides stay untouched.
    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    for source, destination in sorted(sources.items()):
        data = (ROOT / source).read_bytes()
        if source.endswith(".md"):
            description = DESCRIPTIONS[destination]
            data = (
                "---\ndescription: "
                + json.dumps(description)
                + "\n---\n\n"
                + rewrite_links(data.decode(), source, sources)
            ).encode()
        target = OUTPUT / destination
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    (OUTPUT / "license.md").write_text(
        "---\ndescription: Apache-2.0 license and third-party attribution for vacancyd.\n---\n\n"
        "# License\n\nVacancyd is licensed under the [Apache License, Version 2.0]("
        + REPOSITORY
        + "/blob/main/LICENSE-APACHE). Copyright 2026 Maxfield Allison.\n\n"
        "Third-party components retain their own licenses. See [THIRD_PARTY_LICENSES]("
        + REPOSITORY
        + "/blob/main/THIRD_PARTY_LICENSES) and the source notices in each release.\n"
    )
    (OUTPUT / "CNAME").write_text("docs.vacancyd.com\n")
    (OUTPUT / "robots.txt").write_text(
        "User-agent: *\nAllow: /\nSitemap: https://docs.vacancyd.com/sitemap.xml\n"
    )
    print(f"Staged {len(sources)} public source files and license page")


if __name__ == "__main__":
    main()
