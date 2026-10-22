# Releasing vacancyd

A vacancyd release is built, signed and published from a tag in this repository by the maintainer's private release pipeline. This repository holds the source, tests, documentation and the pull-request checks; none of its workflows can sign or publish anything. The pipeline refuses to build a release unless the tag holds exactly the source it is building, and it publishes the same bytes it tested, without rebuilding them. This page describes what a release contains, how it is built and how to verify one.

## Artifacts

Each release contains:

- a Windows x86-64 ZIP and native-service MSI, both including the windowless `vacancyd-session.exe` and `vacancyd-console.exe`, all Authenticode-signed;
- Linux x86-64 and ARM64 tarballs;
- one SHA-256 sidecar for every downloadable package;
- the dependency notices `THIRD_PARTY_LICENSES`, `SBOM.cdx.json` and `THIRD_PARTY_SOURCES.tar.gz`;
- `windows-signatures.json`, the Authenticode verification of the three executables and the MSI;
- `release-manifest.json`, an inventory of every file with its size and SHA-256, the tag and its commit;
- `release-provenance.json`, an SLSA v1 provenance statement naming every file by digest and the tagged commit it was built from;
- `SHA256SUMS`, covering all of the above, and `SHA256SUMS.sigstore.json`, its signature by the project's release key.

The container image is published to Docker Hub and GHCR from the same release; see [the container guide](container.md#how-the-image-is-built).

Windows MSVC builds use the static C runtime configured in `.cargo/config.toml`. Build from the repository root and retain that file in source archives. If you supply `RUSTFLAGS` or other compiler overrides, preserve `-C target-feature=+crt-static`; both binary entry points reject dynamic C runtime linkage. The clean Windows acceptance guest must not need a separately installed Visual C++ Redistributable. See the [Rust linkage reference](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes).

The archives are deterministic when built from the same inputs and `SOURCE_DATE_EPOCH`. They include the binary, configuration example, licenses, changelog, README, and the relevant service documentation.

## Dependency notices

Every archive and MSI includes `THIRD_PARTY_LICENSES`, `SBOM.cdx.json`, and `THIRD_PARTY_SOURCES.tar.gz`. The SBOM is a deterministic CycloneDX 1.6 source inventory for supported targets, including build and development dependencies; it does not claim that every listed crate is linked into every binary. Registry package SHA-256 hashes come from `Cargo.lock`. The release page should publish all three files separately.

After changing dependencies or features, use Rust 1.89 and Python 3.11 or newer:

```sh
cargo install cargo-about --locked --version 0.9.2 --features cli
cargo fetch --locked
python3 scripts/dependency_notices.py
```

Review and commit the lockfile, notices and SBOM together. CI reruns the same generator with `--check` and refuses missing or changed output. `about.toml` is the sole license policy: MIT, Apache-2.0 (including its LLVM exception), BSD-3-Clause, Unicode-3.0, Zlib, MPL-2.0, OFL-1.1, CC0-1.0 and ISC. Where a crate offers alternatives, MIT is preferred; conjunctive obligations remain. These licenses require retaining their applicable notices. Changes to this allowlist require reviewing the dependency and its distribution requirements. Missing, unrecognized and unaccepted license expressions fail generation.

Generation reads packaged license files after the locked fetch. Crates that omit their notices use checksum-pinned Git clarifications retrieved from the exact published source commit recorded inside the locked crate. Generation therefore requires access to those upstream repositories. A canonical text substituted for a missing license file is rejected because it can omit the author's copyright notice. Resolve such a failure with a reviewed, checksum-pinned cargo-about clarification; do not ignore the crate. Generated files omit local paths and timestamps and are compared byte for byte in CI. The SBOM uses the same accepted inventory and Cargo metadata dependency graph, so there is no second dependency policy to keep aligned. Re-run this process when changing console dependencies. Font files and OFL notices are pinned by SHA-256 in `console/font-licenses.json` and checked against the same policy.

The unmodified MPL-2.0 dependency sources are bundled as their exact locked `.crate` archives in `THIRD_PARTY_SOURCES.tar.gz`, available under MPL-2.0 with their existing notices intact. A `.crate` file is itself a gzip-compressed source tar archive. The generator verifies each archive against `Cargo.lock` before including it. The same bundle contains the exact modified Tauri utility crate used for the [upstream dependency backport](dependency-policy.md). Its modified version and source hash are included in the SBOM. The MPL archives supply the covered source alongside the executable as described in [MPL 2.0 sections 3.1–3.3](https://www.mozilla.org/en-US/MPL/2.0/); vacancyd source retains its existing dual license. The SBOM includes the entire Rust workspace and embedded fonts. Frontend compiler/browser-test tools are not embedded in the application.

## Verifying a release

Download the files you want together with `SHA256SUMS` and `SHA256SUMS.sigstore.json` from the release page. The release key's public half is [`packaging/cosign.pub`](../packaging/cosign.pub). With [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) installed:

```sh
cosign verify-blob --key cosign.pub --bundle SHA256SUMS.sigstore.json SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

The first command proves that `SHA256SUMS` was signed by the release key, and cosign also checks the signature's entry in Sigstore's public transparency log. The second proves each downloaded file matches it. Each package's own `.sha256` sidecar is enough to detect a damaged download, but only the signed `SHA256SUMS` shows who published it.

On Windows, the MSI and the three executables also carry Authenticode signatures from the publisher named in `release-manifest.json`, with RFC 3161 timestamps; Windows shows them under the file's **Digital Signatures** properties. For the container image, see [Checking an image](container.md#checking-an-image).

## How a release is built

The public repository on GitHub is the source of truth. A private GitLab project mirrors it and adds the release pipeline, its signing configuration and its credentials; those files never enter the public repository. Pushes to `main` and every `v*` tag are copied into a reserved namespace of that project by the `Sync to GitLab` workflow, which only ever fast-forwards the branch and never moves a tag once copied.

1. **Prepare.** A pull request sets the version everywhere it appears and dates the changelog (the checklist below). When it is merged, the maintainer tags `vX.Y.Z` on `main`.
2. **Prove the source.** The maintainer starts the release pipeline for that version on the private project's protected `main`. Its first job compares the pipeline's commit, less the private release files, with the mirrored tag: every path, every byte and every file mode, in both directions. A file missing from the tag, a file the tag has that the source does not, or any changed byte stops the release before anything is built for it, signed or published. It also checks that every version number and the dated changelog agree with the tag.
3. **Build and test.** The pipeline runs the same formatting, lint, test, dependency-audit, license and secret-scan gates as pull requests, builds the Linux x86-64 and ARM64 archives, and assembles and qualifies the container image from those archives on native amd64 and arm64 hosts. Every container image the pipeline runs is pinned by digest, and a release compiles from the locked dependencies alone, without the build caches that ordinary pipelines share.
4. **Sign Windows.** The maintainer starts the Windows signing job. It starts from a fresh clone and first checks the build host for anything an earlier job could have left behind: configuration files above the checkout, injected compiler or runtime settings, user-level extensions, stray files and processes. Any finding stops the job before it signs. It builds the console's web frontend itself, from the lockfile with a checksum-pinned Node.js, and continues only if that frontend is byte for byte the one the browser tests ran against. It builds the three executables, signs them with Authenticode through Azure Artifact Signing, builds and validates the MSI around the signed files, signs the MSI, and verifies all four with Windows trust against the expected publisher and timestamp. The signing identity is a short-lived workload token that the private CI issues only to this job on the protected branch; there is no client secret, and no other job, branch or merge request can obtain it.
5. **Assemble.** An automatic job accepts exactly the release file set, checks the Windows ZIP's executables against the signature receipts, and opens the container image layer by layer: each platform's image must be the pinned base image, fetched by digest, plus exactly the files of that platform's Linux archive, with the configuration the Dockerfile declares. Every qualification, vulnerability scan and SBOM must name that same image. It then writes `release-manifest.json`, `release-provenance.json` and `SHA256SUMS`. Every published document is checked for private hostnames and addresses.
6. **Publish.** The maintainer approves each publishing job separately. The first signs `SHA256SUMS` with the release key, verifies the signature against `packaging/cosign.pub`, checks that GitHub's tag still names the proven commit, creates the GitHub release as a draft, uploads every file, checks the digest GitHub reports for each, and publishes it. The second copies the qualified image to Docker Hub and GHCR by digest, then signs it and attaches its provenance and SBOM attestations there. A last job downloads every release file and verifies both images the way a user would, without credentials.

Nothing is rebuilt or re-signed after qualification. A missing or expired build output means a new release candidate, not a quiet rebuild. Each publishing job holds only its own credentials, and the pipeline refuses to run a publishing job that finds another job's credential in its environment.

The same pipeline has a rehearsal mode that runs every step on any branch without any credential: it signs with a throwaway key, records the GitHub calls instead of making them, and publishes the image to a throwaway registry that exists only inside the job. It is how changes to the release process are tested.

## Signing and provenance

The release pipeline runs on the maintainer's own GitLab, whose identity tokens Sigstore's public certificate authority does not accept, so keyless signing is not available to it. Releases are signed with a dedicated cosign key pair instead: the private key exists only as a protected variable of the publishing jobs, and the public key is committed as `packaging/cosign.pub`. Signatures are still recorded in Sigstore's public transparency log.

`release-provenance.json` and the image's provenance attestation use the SLSA v1 provenance format. They name the tag, its commit and every output by digest, and they are signed by the release key. They are a statement by the project's release pipeline, not by an independent hosted build service, so they make no claim to a SLSA build level beyond what this page describes.

## Who does what

| Step | Who | Where |
| --- | --- | --- |
| Changes, including the version and changelog | Contributors, reviewed by the maintainer | Pull requests here; the public CI runs on each |
| Tagging `vX.Y.Z` on `main` | Maintainer | This repository |
| Copying `main` and the tag to the private project | `Sync to GitLab` workflow | Automatic on push |
| Starting the release pipeline | Maintainer | Private project |
| Proving the source, building, testing, qualifying, assembling | Release pipeline | Automatic |
| Signing Windows | Maintainer starts the job | Private project |
| Publishing the GitHub release, then the images | Maintainer starts each job | Private project |
| Verifying the public downloads and images | Maintainer starts the job | Private project |

## Release checklist

1. Move the relevant entries from `Unreleased` to a dated version in `CHANGELOG.md`.
2. Set the same version in `Cargo.toml`, then run `cargo check` so `Cargo.lock` records it. Align `console/Cargo.toml`, `console/tauri.conf.json`, and the frontend package metadata with the same release version. Regenerate notices.
3. Run the full local verification commands from `CONTRIBUTING.md`. Include the read-only `scripts/check_msi.ps1` artifact check on Windows; it verifies the action table directly rather than trusting decompiled scheduling.
4. Exercise MSI install, service start, upgrade, and uninstall on a disposable Windows VM. Confirm an existing `C:\ProgramData\vacancyd\vacancyd.toml` is retained. Include the supported cases from the [session-agent lifecycle matrix](agent-startup.md), running-agent upgrades, failed-upgrade rollback, uninstall cleanup, disabled startup preferences and prepublic task migration. Before claiming RDP reconnect or simultaneous active-session support, extend that evidence to two interactive users on Windows Server. Run unattended MSI operations with `/norestart`; major upgrades and uninstall return `3010` to require a reboot. Verify session versions and cleanup after that reboot. The required `ScheduleReboot` action is incompatible with a Windows Installer multiple-package transaction; test the standalone MSI.
5. Exercise the systemd install and an admitted request on Linux x86-64 with `scripts/qualify_linux.py` on a disposable host. It installs the exact archive, exercises systemd startup, a real admitted request through a local fixture, disabled lending, restart and removal, and refuses a host that already has vacancyd installed. Before promoting ARM64 from preview to supported, repeat it on physical ARM64 hardware. Review the [v1 compatibility contract](compatibility.md) against the exact candidate, including configuration defaults, status schema and reporter version matching. Any incompatible change needs an explicit version and migration decision before publication.
6. Merge, tag `vX.Y.Z` on `main`, and run the release as described above.
7. Once the release is public, check every repository and installer URL in the release notes and documentation.

Ordinary branch and pull-request CI builds are unsigned development builds. Public releases are built and signed only after the release pipeline verifies their tagged source.
