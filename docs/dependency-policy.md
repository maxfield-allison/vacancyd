# Dependency review and the Tauri URLPattern repair

vacancyd scans the complete Cargo lockfile with pinned cargo-audit, retains the unfiltered report, and checks reachability on all three supported build targets. Any reported vulnerability fails the gate. An advisory for an unsupported platform can be dispositioned only while the package is absent from every supported build graph, including build and development dependencies. Those exact dispositions expire and are rechecked on every run. There is no accepted maintenance exception for a dependency used by a supported build.

## Remove UNIC rather than suppress its warnings

The published Tauri 2.11.5 / tauri-utils 2.9.3 chain requires URLPattern 0.3.0, which pulls in five unmaintained UNIC 0.9.0 crates. Their RustSec advisories are maintenance findings, not proof of an exploitable vulnerability. Nevertheless, the release policy requires removing this chain before v1.

Tauri's own [dependency update](https://github.com/tauri-apps/tauri/commit/dd725f4b13c30a86b398ccc59eb498f151f461c5) changes URLPattern from 0.3 to 0.6 without modifying Tauri library code. URLPattern 0.6 uses maintained ICU properties. At the v1.0.0 dependency freeze, this update was not published in stable tauri-utils (checked September 16, 2026).

The candidate carries that exact dependency update against the published tauri-utils 2.9.3 source. `vendor/tauri-utils` is labeled `2.9.3+vacancyd.1` so the local modification is visible. No Rust or JavaScript code in that library is changed. The original upstream crate archive is retained under `vendor/upstream`; its checksum matches the original registry lock entry. `policy/vendor.json` records the original source revision and the complete permitted diff. `scripts/check_vendor.py` verifies every file against that archive. Only the two manifest dependency/version changes are allowed; the old standalone development lockfile is omitted in favor of the application lockfile. Notices and the SBOM identify the modified package, and the source bundle carries the exact modified source used in the build.

This is a maintained local backport, not an upstream stable release. Its maintenance obligation is explicit: monitor stable Tauri utility releases, remove the override when a compatible stable version includes the fix, and repeat the dependency audit, library tests, native console and installer checks. Do not expand the patch to unrelated upstream changes or switch to Tauri 3 alpha to avoid reviewing this obligation.

## Alternatives considered

| Route | What it removes | Additional responsibility |
| --- | --- | --- |
| Exact upstream dependency backport | All five UNIC crates; preserves the approved UI and current host | Verify the small permitted diff and retire the override when stable upstream includes it |
| Wry or direct WebView2 host | The Tauri application framework and URLPattern path | Own origin checks, IPC boundaries, local asset serving and host lifecycle; repeat native security and accessibility qualification |
| Native Windows UI using WinUI or WPF | The web renderer and its JavaScript bridge | Rebuild the presentation and qualify a different framework, toolchain, accessibility behavior and package lifecycle |

The latter options are possible, but removing a framework does not by itself prove a safer result. For the known dependency finding, the exact upstream backport is the smallest repair to validate. A native UI remains an alternative if the current console fails its existing Windows acceptance gates. The service and session reporter remain independent of this presentation choice.

References: [RustSec finding and suggested replacements](https://rustsec.org/advisories/RUSTSEC-2025-0100.html), [URLPattern's ICU replacement](https://github.com/denoland/rust-urlpattern/commit/b047afee9b901e19928f87470ba722bbac05a27f), [Wry source and supported hosts](https://github.com/tauri-apps/wry), [Windows desktop frameworks](https://learn.microsoft.com/en-us/windows/apps/desktop/).

## Windows notification-area controls

The console enables Tauri 2.11.5's tray-icon feature, resolving tray-icon 0.24.2. The lockfile adds its cross-platform support chain without changing existing crate versions. The conservative inventory also includes libappindicator 0.9.0, libappindicator-sys 0.9.0 and libloading 0.7.4 even though the console is built only for Windows. libloading's packaged ISC copyright and permission text was reviewed and is retained in generated notices; ISC is added to the accepted license set for this dependency. No crate is ignored or exempted from scanning.

Tray implementation follows [Tauri's system tray API](https://tauri.app/learn/system-tray/). Session-local single-instance ownership uses the same Windows mutex pattern as the existing reporter, with a separate name and process lifetime.

`libappindicator-sys` omits its license file and repository URL from the registry archive. Both 0.9.0 crates record source revision `eafd1e3682a1247f595410266091e9684021cb6f`; the sibling `libappindicator` archive includes that repository's root MIT notice. The checksum-pinned cargo-about clarification reads this exact sibling text, retaining the qDot and Tauri Apps copyrights. Both archive checksums remain locked; neither package is excluded.
