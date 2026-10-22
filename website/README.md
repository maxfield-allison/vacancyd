# vacancyd websites

The app page and documentation share this repository. The app page is a static Cloudflare Pages site at **https://vacancyd.com/**. GitHub Pages builds the guides at **https://docs.vacancyd.com/**. Their deployment state is separate from the application release.

## Local checks

Use Python 3.13 and the console's locked Node dependencies:

```sh
python -m venv .venv-docs
.venv-docs/bin/python -m pip install --require-hashes -r requirements-docs.txt
python scripts/build_website.py --preview
.venv-docs/bin/python scripts/build_docs.py --preview
python scripts/check_sites.py
npm --prefix console/web ci --ignore-scripts --no-audit --no-fund
cd console/web
npx --no-install playwright test --config=playwright.website.config.mjs
```

Preview builds omit analytics and never invent release download URLs. Production app builds query GitHub's published stable release, requiring the Windows MSI, Linux archive and signed checksum files before showing downloads. A tag without a published release leaves the source-and-guides action in place. A transient API error fails the build, preserving the previous deployment. Rebuild the app site after publishing a release to refresh its download links.

## Deployment

For Cloudflare Pages, connect this repository, select `main` as the production branch, leave the repository root unchanged, use `python3 scripts/build_website.py` as the build command and `build/website` as the output directory. No build token or runtime secret belongs in this repository. Set up `vacancyd.com` through the Pages project's Custom domains screen so Cloudflare owns the appropriate apex record. Redirect the project's `pages.dev` hostname to the canonical custom domain after it is active.

Enable GitHub Pages with **GitHub Actions** as the source. `.github/workflows/pages.yml` builds and deploys the guides with Zensical, using the native `zensical.toml` configuration and hash-locked documentation toolchain. Configure `docs.vacancyd.com` as the custom domain and add a DNS-only CNAME pointing to `maxfield-allison.github.io`. Verify the domain and certificate, then enable HTTPS enforcement. The generated documentation includes its `CNAME` file.

Do not add `www`, wildcard records, new accounts or paid services as part of either setup. Existing unrelated DNS records stay unchanged.

## Analytics and assets

`analytics.json` contains public Umami Website IDs, not credentials. Production pages use the existing public tracker proxy at `https://probablyfine.dev/pf-insights/script.js`, restricted to the correct hostname. Do Not Track is respected. Preview domains and localhost do not send pageviews. Check both the browser's collection response and the corresponding Umami website's stored visit before treating delivery as verified.

The brand mark and bundled Archivo/IBM Plex Mono fonts come from this project. Font licenses are copied with the website. `assets/social-card.png` is original project artwork; the two console images render the real frontend with synthetic example data. Their provenance files record the source and hashes. They do not show a user's machine, prove native application qualification or contain measured performance claims. Regenerate using `console/web/site-tests/capture-brand.mjs` and `capture-console.mjs` from `console/web`; the console capture expects its local test server on port 38110.

Both sites carry canonical URLs, descriptions, crawl metadata and social previews. `scripts/check_sites.py` checks their rendered links, anchors, assets and sitemap targets. These checks do not establish indexing, ranking, real assistive-technology conformance or production HTTPS.
