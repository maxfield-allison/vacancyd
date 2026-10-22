# Contributing to vacancyd

Everyone taking part in the project is expected to follow the [code of conduct](CODE_OF_CONDUCT.md). Report a suspected vulnerability privately as described in [SECURITY.md](SECURITY.md), not in a public issue.

vacancyd builds with Rust 1.89 or newer and needs Python 3.11 or newer for the release tooling. Normal development does not require Python.

CI checks formatting, lints and tests with Rust 1.89.0 exactly. Each Rust release adds clippy lints, so a newer clippy can report findings that CI never sees, and rustfmt output can differ between releases. Install the same toolchain beside your usual one:

```text
rustup toolchain install 1.89.0 --profile minimal --component rustfmt --component clippy
```

Before submitting a change, run:

```text
cargo +1.89.0 fmt --all -- --check
cargo +1.89.0 clippy --locked --all-targets --all-features -- -D warnings
cargo +1.89.0 test --locked --all-targets --all-features
python -m unittest discover -s scripts -p 'test_*.py'
python scripts/check_vendor.py
```

Findings from a newer clippy are welcome fixes when the suggested code also builds on 1.89; the 1.89.0 run is the one that has to pass. The repository does not pin the toolchain with a `rust-toolchain.toml`, so building from source with any Rust 1.89 or newer keeps working offline and never downloads a second toolchain.

Pull requests run the same checks in GitHub Actions (`.github/workflows/ci.yml`): formatting, clippy, tests, the dependency audit and license notices, a secret scan, the console's browser tests and the native Windows tests, plus CodeQL. None of those workflows holds a secret or can publish anything.

The ignored hardware tests are intentional and name their prerequisites. Run them on a Windows machine with a WDDM/NVIDIA adapter before changing GPU identity or attribution code:

```text
cargo test --locked -- --ignored --nocapture
```

Changes to admission, presence, pressure, authentication, or readiness should include a test for the state that caused the old behavior and should preserve the rule that an unknown or stale safety input cannot authorize lending.

Releases are built, signed and published by the maintainer from a tag on `main`; contributors never need signing keys or registry access. See `docs/releasing.md` for what a release contains, how it is built and how to verify one.

Build Windows packages with `cargo build --locked --release --features windows-session`. This includes the GUI-subsystem background agent alongside the console CLI. Lifecycle tests send shutdown messages only to their own hidden test windows; they never register application restarts or request an actual OS shutdown. Logon, multi-user, upgrade and uninstall behavior require a disposable Windows VM; do not change startup entries or install test MSIs on a daily-driver workstation.

After building an MSI, run `scripts/check_msi.ps1 -MsiPath <package.msi>` on Windows. This opens the MSI database read-only and verifies actual upgrade sequencing, machine scope and shortcut ownership. It does not replace ICE validation or the disposable-VM lifecycle matrix. WiX decompilation alone is not sequencing evidence.
