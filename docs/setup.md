# First-run setup

`vacancyd setup` configures one dedicated borrowing upstream with an NVIDIA GPU or system memory as its primary resource, plus CPU and RAM capacity checks. It writes TOML for you, validates it, and creates a separate borrow-only token. It does not install an inference server, download models, restart vacancyd, change the firewall, or register the desktop agent.

This is a Windows command-line onboarding step, not the finished one-click experience. On Linux and macOS, setup refuses to proceed. A Linux lender is configured by hand from `vacancyd.example.toml` and installed with `contrib/vacancyd.service`, as described in [Linux: install from the release archive](#linux-install-from-the-release-archive): presence comes from systemd-logind with no desktop agent, `owner.gpu_pressure.enabled` must be false, and a host without systemd-logind sets `owner.presence = "headless"`. macOS has no presence backend. `discover` is available independently on both; finding a GPU does not establish that lending is supported.

## Before setup

Use an inference-server instance dedicated to borrowed work, bound only to loopback. Do not share it with owner jobs: process attribution cannot separate owner and borrower requests inside the same process. A loopback URL alone does not prove the server is inaccessible from the LAN. Verify the actual listener and test from another machine that the real service port cannot be reached.

Measure the largest model/job footprint you will admit, including headroom. Setup requires this value in MiB; it cannot infer a safe value from the GPU's size. Its single default applies to every NVIDIA GPU dependency, or, in a CPU-only setup, to system memory on top of the RAM reserved for the owner. A scalar model entry applies the same MiB value to the same resources. For a split model, use a per-resource map such as `"split-model" = { gpu0 = 10000, gpu1 = 14000 }`; name every NVIDIA GPU dependency explicitly. Use a manually reviewed multi-resource configuration instead of this wizard.

The chosen GPU UUID is a dependency declaration, not a command that pins the server to that card. Configure the dedicated upstream process separately. For CUDA servers, set `CUDA_VISIBLE_DEVICES=GPU-...` to the selected stable UUID where supported, then load a test model and use `nvidia-smi` to verify its process is on that UUID. CUDA may renumber visible devices inside the process, so do not equate an internal numeric device index with the host's NVIDIA index.

The generated local API trusts loopback clients for reading. On Windows a loopback client without a token may also pause lending, and nothing else: resuming another account's pause, switching resources, releasing held work and saving settings need an elevated administrator, which the owner console asks for through the Windows approval prompt. Linux applies the same limit; there those changes need root or a member of the `vacancyd-admin` group, through `vacancyd admin-request`. On macOS loopback clients can still administer lending in this release, so that profile is for a trusted workstation, not an untrusted multi-user host. See [authentication](authentication.md) for the local authentication policy. A remote borrower gets only `borrow`, not status or administration access. This is not an owner-priority credential; remote owner-priority borrowing remains unimplemented.

## Run

For the MSI's unmodified starter, right-click **Setup (run as administrator)** in the **vacancyd** Start menu folder and choose **Run as administrator**. Windows asks for administrator consent or credentials. Opening the shortcut normally does not elevate it and cannot access the protected configuration. You can also use an elevated PowerShell prompt:

```powershell
& "$env:ProgramFiles\vacancyd\vacancyd.exe" setup `
  --config "$env:ProgramData\vacancyd\vacancyd.toml" --replace-starter
```

For a development configuration, choose a new file. On Windows it must be inside `ProgramData\vacancyd`, for example in a folder of its own:

```powershell
& "$env:ProgramFiles\vacancyd\vacancyd.exe" setup `
  --config "$env:ProgramData\vacancyd\lender\vacancyd.toml"
```

Setup does not elevate itself. Existing customized configs are never replaced, even with `--replace-starter`; choose a new path and review the differences. Do not edit the destination or its parent directory concurrently with setup. Only trusted accounts should have write access to that directory. On Windows, setup writes only inside `ProgramData\vacancyd`, the folder the MSI protects for Administrators and LocalSystem, and refuses any other destination with an error that names it and that folder. It checks the destination directory whether or not it exists: no folder on the way may be a junction or other reparse point, and from `ProgramData\vacancyd` down every folder must give access only to Administrators and LocalSystem. The protected root must already exist; the MSI creates it. For a source or ZIP installation, create and protect it as described in the [manual service procedure](windows-service.md#procedure) before running setup. Setup creates missing directories below it with private permissions.

Discovery checks the local NVIDIA inventory and three loopback HTTP endpoints: Ollama `11434`, OpenAI-compatible `8080`, and ComfyUI `8188`. HTTP probes bypass environment proxies, refuse redirects, cap response bodies at 64 KiB, and have two-second deadlines; NVIDIA inventory has a five-second deadline. These probes do not load a model. Missing GPU inventory is reported as an error, not as an idle GPU. The separate `vacancyd discover` command prints the same results as JSON without creating files. Results are candidates, not proof of isolation or compatibility with every inference route.

Setup never edits the upstream service or firewall. After activation, status and the console classify the real upstream listener as loopback only, listening on a network address, or unknown. A network-bound direct listener lets direct requests bypass owner protection; it does not by itself prove Internet exposure. An unknown result still needs manual bind verification. A target that is not on this machine (another host, or any host name other than `localhost`, such as a container service name) is reported as not observable: its bind has to be checked where it runs.

Setup asks for:

- GPU or CPU-only operation, dialect, and loopback target URL. The URL may not contain credentials; an upstream that needs one is given `[upstream.credential]` afterwards (see [Authentication](authentication.md)).
- Confirmation that the upstream is a dedicated borrower instance.
- Proxy address, defaulting to loopback. Select a specific LAN IP deliberately to permit remote borrowing. HTTP bearer tokens are not encrypted in transit; restrict access to trusted clients/networks. Never expose this port directly to the Internet.
- Required free VRAM per job with a GPU, or free RAM per job beyond the owner's reserve when CPU-only, and RAM reserved for the owner. A job is admitted only while both its requirement and the reserve fit in what is free, and setup refuses a budget larger than the machine could ever hold. CPU capacity uses an 85% utilization ceiling. These are admission checks, not OS quotas.
- GPU pressure mode: **calibrated** by default, which learns each GPU's ordinary load, or **fixed** with an explicit measured threshold.
- Default 30-second drain, with optional advanced immediate/configurable/unlimited choices, then acceptance of upstream limitations and confirmation to write.

Presence blocks new work immediately. Accepted proxy work can finish for up to 30 seconds, after which its upstream connection is cancelled. Advanced settings in each `[[upstream]]` are `on_owner_return = "cancel"` for immediate cancellation, `"drain"` with `drain_timeout_s = 30` (nonnegative whole seconds), or `"finish"` for completion without an owner-return timeout.

These same options govern GPU-pressure reclaim for work using the affected resource. Other resources keep running, and repeated reclaim events cannot extend the first deadline. A pressure reading that is only unavailable reclaims nothing until the GPU has gone without a good sample for at least 20 seconds (one missed sample is tolerated), and a drain whose trigger clears before its deadline (the owner leaves, or the pressure reading recovers) stands down without cancelling anything. Zero seconds in drain mode is immediate cancellation. All three modes refuse new work while the owner is present, on every upstream the owner veto covers. An upstream set to `owner_scope = "resource"` is exempt and keeps lending; see [resource-scoped upstreams](admission.md#resource-scoped-upstreams). Restart the service after editing config.

Cancelling a proxy connection does not prove the upstream stopped computing. ComfyUI may already have queued a workflow, so vacancyd also deletes or stops the ComfyUI prompts it admitted, except in finish mode. Cancellation requests name only borrowed prompt IDs. ComfyUI 0.3.57 to 0.25.x still has an interrupt race with the next prompt; 0.26.0 and later cancels under the queue lock. Stopping a running prompt needs ComfyUI 0.3.57 or later; see [compatibility](compatibility.md#comfyui-releases). OpenAI-compatible servers choose their own disconnect behavior; Ollama model residency is separate from its response stream. Setup leaves automatic unloading off because it cannot prove job cancellation. With `yield_on_veto = true`, supported adapters receive unload requests immediately in cancel mode, when work ends or at the drain deadline in drain mode, or when work ends or `completion_timeout_s` passes in finish mode. ComfyUI is not unloaded while it runs or queues prompts vacancyd did not admit. See [architecture](architecture.md#yield-srcyield_controlrs). No files are written on prompt cancellation or EOF.

## What is written

The destination contains `vacancyd.toml` (or your chosen name) and a randomly named `vacancyd-borrow-*.token`. The config refers to the token by relative filename, never by its value. When replacing the MSI starter in a directory named `vacancyd` on Windows, setup protects the configuration, token and backup for Administrators and LocalSystem and assigns their ownership to Administrators. This supports the MSI's LocalSystem service; a custom service account needs a reviewed permission change. Other setup destinations use a protected file ACL that also grants the file owner access, and setup does not reassign their ownership. Do not put credentials into source control, chat, command arguments, logs, or screenshots. Transfer the token using your secret-management workflow.

Setup validates the staged config and resolves its credential using the same load/authentication checks as `check-config` before publishing it. New configs are published without replacing a racing writer's file. Replacing the exact MSI starter preserves its original bytes in `vacancyd-before-setup-*.toml`. Failures before publication leave the previous config intact and remove newly created transaction files when cleanup succeeds. The directory must support hard links for publishing a new file (use a local NTFS volume on Windows).

## Verify and activate deliberately

Read the generated config. Check the actual service binding, GPU selection and model footprint, transfer the borrow token securely to your gateway, and review any needed firewall rule. Keep the API on loopback; only the proxy needs a borrower-scoped inbound rule. Then validate and restart explicitly:

```powershell
& "$env:ProgramFiles\vacancyd\vacancyd.exe" check-config `
  --config "$env:ProgramData\vacancyd\vacancyd.toml"
Restart-Service vacancyd
& "$env:ProgramFiles\vacancyd\vacancyd.exe" status
nvidia-smi
```

During the test load, match the process to the configured `GPU-...` UUID rather than assuming that device `0` means the same adapter in every tool or process.

Setup keeps `unlocked_session_is_presence = true` and `idle_lends_after_s = 0`. An unlocked workstation therefore refuses borrowing even after calibration. Calibration completion is not evidence that presence protection is unnecessary; setup never relaxes it automatically. See [calibration](calibration.md) for progress, separate attended/unattended profiles, hints and reset instructions.

The MSI installs [session-agent logon startup](agent-startup.md). In your current session, choose **vacancyd > Start session agent** without elevation. Its idle and foreground signals become necessary if you later choose an idle-lending policy. Portable ZIP deployments need their own startup integration. Installer rollback, upgrade restart and multi-user execution still need disposable-VM acceptance.

## Recovery

For rollback, stop vacancyd, preserve the generated config, restore the saved starter to the configured path, and restart explicitly. The starter lends nothing. Keep the generated token and config together if you may restore them; uninstall does not revoke or delete them. Revoke the gateway credential before discarding a setup you have used.

An interrupted process or failed cleanup can leave `.vacancyd-setup.lock`, a staged `.vacancyd-setup-*.toml`, or private token/backup files. First confirm no setup process is still running. Inspect the final config with `check-config` before activating anything. Remove only the stale lock and transaction files you have identified; do not remove the token referenced by the active config. A retained lock blocks another setup until cleared. The lock coordinates setup processes, not external editors, and this is not a power-loss recovery journal.

## Linux: install from the release archive

Setup does not run on Linux, so a Linux lender starts from the example configuration. The release archive `vacancyd-<version>-x86_64-unknown-linux-gnu.tar.gz` extracts to one directory holding the `vacancyd` binary, `vacancyd.example.toml`, `contrib/vacancyd.service` and these docs. Verify the archive against its `.sha256` sidecar, extract it, and run every command below from that directory:

```sh
sha256sum -c vacancyd-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf vacancyd-<version>-x86_64-unknown-linux-gnu.tar.gz
cd vacancyd-<version>-x86_64-unknown-linux-gnu
sudo install -m 755 vacancyd /usr/local/bin/
sudo install -d -m 755 /etc/vacancyd
sudo install -m 600 vacancyd.example.toml /etc/vacancyd/vacancyd.toml
```

From a source checkout, build with `cargo build --locked --release` and install `target/release/vacancyd` instead of `vacancyd`; the other paths are the same.

The example describes a Windows workstation. Edit `/etc/vacancyd/vacancyd.toml` (for example with `sudoedit`) before starting anything:

- Set `enabled = false` under `[owner.gpu_pressure]`. Linux has no GPU attribution backend yet, and left enabled it keeps GPU lending refused. To keep calibration state for later, set its `state_file` to `"/var/lib/vacancyd/pressure.json"`.
- Uncomment `admission_state_dir = "/var/lib/vacancyd/admission"` under `[server]`. The unit makes `/var/lib/vacancyd` writable; the rest of `/etc` is read-only to the service. With it, a manual `sudo vacancyd run` uses the same recovery records as the service. Without it, a manual run refuses to start if the service's recovery directory exists and holds records of unfinished or held work. It warns and continues if that directory exists without records or cannot be read. If the directory does not exist, this check says nothing. See [admission](admission.md).
- Presence comes from systemd-logind. On a host without it, set `owner.presence = "headless"` and `processes = []`.
- Replace `processes` with the programs that mean you are using the machine.
- Replace the `[[resource]]` selectors with your own GPU UUIDs (`nvidia-smi -L` or `vacancyd discover`), and delete resources you do not lend.
- Point each `[[upstream]]` at your own inference server, bound to loopback, and delete the ones you do not run. Every resource an upstream names must be defined and enabled.
- The example listens on loopback only. To lend to other machines, add tokens before changing any `listen` address; see [authentication](authentication.md). A token file for the service can live beside the configuration: `sudo sh -c 'umask 077; vacancyd gen-token > /etc/vacancyd/cluster.token'`, then `value_file = "/etc/vacancyd/cluster.token"`.

Validate the result, then install and start the service. The shipped unit lets the service write `/etc/vacancyd`, `/var/lib/vacancyd` and `/run/vacancyd`, plus its private `/tmp` and `/var/tmp` under `PrivateTmp=true`. Files in `/etc/vacancyd` and `/var/lib/vacancyd` survive service restarts. `/run/vacancyd` is for runtime files and is removed when the service stops. The private `/tmp` and `/var/tmp` don't survive service restarts, so persistent state doesn't belong there. A directory outside all of the unit's writable paths needs a `ReadWritePaths=` drop-in (`sudo systemctl edit vacancyd`). `check-config` checks the paths needed for `admission_state_dir`, `state_file`, `admin_socket` and saving the configuration, warns about those the unit would block, and names the drop-in line. When the unit is installed and loaded, the check models its settings, including drop-ins. Before installation it uses the shipped unit.

```sh
sudo vacancyd check-config --config /etc/vacancyd/vacancyd.toml
sudo install -m 644 contrib/vacancyd.service /etc/systemd/system/
sudo systemd-analyze verify /etc/systemd/system/vacancyd.service
sudo systemctl enable --now vacancyd
vacancyd status
```

**Upgrading an existing install.** Previously copied units may contain `Environment=RUST_LOG=info`, which overrides `[log] level` in the configuration. A unit you copied before is not replaced by an upgrade, so delete that line, or install the new unit as above, then reload and restart:

```sh
sudo sed -i '/^Environment=RUST_LOG=info$/d' /etc/systemd/system/vacancyd.service
sudo systemctl daemon-reload
sudo systemctl restart vacancyd
```

`vacancyd status` prints `logging: RUST_LOG filter in force` while the line is still there. A drop-in that sets `RUST_LOG` on purpose keeps working the same way.

To let an account change settings and switches without `sudo`, add it to the `vacancyd-admin` group before starting the service (membership applies from its next login; root always can):

```sh
sudo groupadd --system vacancyd-admin
sudo usermod -aG vacancyd-admin <account>
```

`vacancyd status` lists your resources and upstreams, and `curl -s http://127.0.0.1:9811/status` reports `"presence_backend": "logind"` (or `"headless"`). `nvidia-smi` must be on the service's `PATH`. See [local administration on Linux](authentication.md#local-administration-on-linux) for `vacancyd admin-request`.

## Protected Windows configuration and migration

The MSI restricts its ProgramData directory and configuration to Administrators and LocalSystem. Standard users run the reporter and the owner console without reading credentials. From the console they can read status and settings and pause lending; saving settings and the other changes that need an administrator raise the Windows approval prompt. Run setup from an elevated prompt; its staged config, credential and backup are protected before the config is published. LAN-bound daemon startup refuses a config that does not pass the installed permission check. The check reads what the permissions grant, not their exact text: the owner must be Administrators or LocalSystem, and every entry that grants access, explicit or inherited, must name one of them. A config an elevated editor saved by replacing the file, which inherits the directory's permissions, passes. On a first install, the MSI refuses to adopt an existing `vacancyd.toml` that Administrators or LocalSystem do not own, because an ordinary user could have created it before the install. Review such a file, then remove it or run `protect-config` yourself, and install again.

To copy an existing manual installation into the MSI directory, keep the original service stopped and use the elevated `migrate-config` command:

```powershell
$source = Read-Host "Full path to the existing configuration"
& "$env:ProgramFiles\vacancyd\vacancyd.exe" migrate-config --source $source --config "$env:ProgramData\vacancyd\vacancyd.toml" --replace-starter
```

Migration preserves the original config bytes and copies sibling credential files with their names unchanged. It accepts a new destination or the exact unmodified MSI starter. Migration checks the path before creating missing destination directories and parents, and creates them private from the start. It refuses a path that fails those checks. On Windows the configuration's destination directory must be `ProgramData\vacancyd` itself or a folder named `vacancyd` inside it. The protected root must already exist. Migration checks an existing destination directory as it does a new one, before changing its permissions. It refuses a directory outside that root or one whose final folder name is not `vacancyd`. It refuses to overwrite another configuration or credential, traverse nested relative credential paths, or publish before validation succeeds. Absolute credential paths stay external and need protected permissions already; environment-backed credentials still depend on the service environment. A missing credential or permission failure leaves the prior config in place and removes files created by the failed transaction. Before copying anything, migration also refuses if the source's admission recovery directory holds records of unfinished work and the copy would resolve `server.admission_state_dir` somewhere else, as an empty or relative setting does when the configuration moves. The service would otherwise start without them. Set `admission_state_dir` in the source to the absolute path the refusal names, then migrate again. On Windows, if that recovery directory is outside `ProgramData\vacancyd`, keep every vacancyd daemon stopped and move the whole directory, preserving its contents, into the protected folder first. Make its owner and access private to Administrators and LocalSystem, and set `admission_state_dir` in the source to its new absolute path before migrating. Do not replace existing recovery records with an empty directory. The source is retained for rollback. Nothing starts or restarts a service.

Compare the source and destination SHA-256 values without displaying either file. Retain the protected starter backup until the new installation has passed its own acceptance checks. Upgrade, rollback and uninstall preserve the operator configuration; tightening its access does not restore the earlier broad permissions on rollback.

`protect-config --config <path>` repairs permissions without rewriting config bytes. It accepts only a dedicated directory named `vacancyd`, refuses reparse points and multiply-linked files, and changes ownership as well as access so an old ordinary owner cannot restore broad permissions. Use it from an elevated prompt after reviewing the exact target path. A failed repair must be resolved before LAN lending is enabled. The installer runs it with `--require-admin-owner`, which refuses a file whose owner is not Administrators or LocalSystem instead of taking ownership of it.
