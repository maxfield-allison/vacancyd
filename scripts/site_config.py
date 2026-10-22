"""Public website configuration; no authentication credentials belong here."""

from pathlib import Path
import json
import uuid

ROOT = Path(__file__).resolve().parent.parent
TRACKER = "https://probablyfine.dev/pf-insights/script.js"


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
    return (
        f'<script defer src="{TRACKER}" data-website-id="{identifier}" '
        f'data-domains="{domain}" data-do-not-track="true" '
        'data-exclude-search="true" data-exclude-hash="true"></script>'
    )
