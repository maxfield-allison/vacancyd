"""Public website configuration; no authentication credentials belong here."""

import html
import json
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def analytics_id(site: str, preview: bool) -> str:
    if preview:
        return ""
    path = ROOT / "website" / "analytics.json"
    if not path.exists():
        raise ValueError(
            "website/analytics.json is required for a tracked production build"
        )
    identifier = json.loads(path.read_text())[site]
    if str(uuid.UUID(identifier)) != identifier:
        raise ValueError("Umami Website IDs must be canonical public UUIDs")
    return identifier


def tracker_tag(identifier: str, domain: str) -> str:
    if not identifier:
        return ""
    if domain not in ("vacancyd.com", "docs.vacancyd.com"):
        raise ValueError("Unexpected product hostname")
    values = json.loads((ROOT / "website" / "analytics.json").read_text())[
        "campaign_values"
    ]
    attributes = {
        "src": "/assets/javascripts/analytics.js",
        "data-product": "vacancyd",
        "data-website-id": identifier,
        "data-app-host": "vacancyd.com",
        "data-docs-host": "docs.vacancyd.com",
        "data-docs-prefix": "/",
        "data-surface": "docs" if domain == "docs.vacancyd.com" else "site",
        "data-utm-values": json.dumps(values, separators=(",", ":")),
    }
    rendered = " ".join(
        f'{key}="{html.escape(value, quote=True)}"' for key, value in attributes.items()
    )
    return f"<script defer {rendered}></script>"
