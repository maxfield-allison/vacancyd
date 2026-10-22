#!/usr/bin/env python3
"""Check built app/docs links, image targets, canonical hosts and crawl metadata."""

from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urljoin, urlsplit
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
HOSTS = {"vacancyd.com": ROOT / "build/website", "docs.vacancyd.com": ROOT / "site"}


class Document(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.ids = set()
        self.links = []
        self.canonical = None
        self.h1 = 0
        self.description = None
        self.feed(text)

    def handle_starttag(self, tag, attributes):
        attrs = dict(attributes)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag == "h1":
            self.h1 += 1
        if tag == "link" and attrs.get("rel") == "canonical":
            self.canonical = attrs.get("href")
        if tag == "meta" and attrs.get("name") == "description":
            self.description = attrs.get("content")
        for attribute in ("href", "src"):
            if attrs.get(attribute):
                self.links.append(attrs[attribute])


def target_file(host, path):
    target = HOSTS[host] / unquote(path).lstrip("/")
    if not target.resolve().is_relative_to(HOSTS[host].resolve()):
        raise ValueError(f"Link escapes its site: {path}")
    return target / "index.html" if target.is_dir() else target


def main():
    errors = []
    documents = {
        path: Document(path.read_text())
        for folder in HOSTS.values()
        for path in folder.rglob("*.html")
    }
    for host, folder in HOSTS.items():
        for path, document in documents.items():
            if not path.is_relative_to(folder) or path.name == "404.html":
                continue
            relative = path.relative_to(folder).as_posix()
            canonical = f"https://{host}/" + relative.removesuffix("index.html")
            for valid, message in (
                (document.canonical == canonical, "canonical"),
                (document.h1 == 1, "one primary heading"),
                (bool(document.description), "description"),
            ):
                if not valid:
                    errors.append(f"{host}/{relative}: missing or invalid {message}")
            for link in document.links:
                url = urlsplit(urljoin(canonical, link))
                if url.hostname not in HOSTS:
                    continue
                target = target_file(url.hostname, url.path)
                if not target.is_file():
                    errors.append(f"{host}/{relative}: missing target {link}")
                elif (
                    url.fragment
                    and target in documents
                    and unquote(url.fragment) not in documents[target].ids
                ):
                    errors.append(f"{host}/{relative}: missing anchor {link}")
        robots = (folder / "robots.txt").read_text()
        if f"Sitemap: https://{host}/sitemap.xml" not in robots:
            errors.append(f"{host}: robots sitemap reference missing")
        sitemap = ET.parse(folder / "sitemap.xml")
        for location in sitemap.findall(
            ".//{http://www.sitemaps.org/schemas/sitemap/0.9}loc"
        ):
            url = urlsplit(location.text)
            if url.hostname != host or not target_file(host, url.path).is_file():
                errors.append(f"{host}: invalid sitemap URL {location.text}")
    if errors:
        raise SystemExit("\n".join(errors))
    print(
        f"Checked {len(documents)} rendered documents: canonical hosts, headings, descriptions, links, anchors, assets and sitemaps"
    )


if __name__ == "__main__":
    main()
