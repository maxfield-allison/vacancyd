# Running vacancyd in a container

The container image is for a Linux machine that lends its hardware and already runs its inference servers in containers: a dedicated GPU box, a homelab node, a server nobody sits at. It runs vacancyd as an unprivileged user with the configuration mounted read-only. On a desktop you use, the systemd service from the Linux archive is simpler; the image can read desktop presence too, but only with the extra host access described under [Desktop presence](#desktop-presence-logind).

```text
docker.io/probablyfinedev/vacancyd
ghcr.io/maxfield-allison/vacancyd
```

Both registries carry the same image, by digest. Release tooling refuses to overwrite a `vX.Y.Z` tag with different content; pin the image digest for an immutable reference. It moves `latest` when it is the newest release, so publishing a fix for an older line never moves `latest` backwards. The image is signed; [Checking an image](#checking-an-image) shows how to verify it. The image covers `linux/amd64` and `linux/arm64`; ARM64 has the same preview status as its archive (see the platform table in the README).

## What is in the image

The image does not compile vacancyd. It is assembled from the release's Linux archives: the `vacancyd` binary in the image is the one in `vacancyd-X.Y.Z-<target>.tar.gz`, checked against that archive's SHA-256 sidecar while the image is built. Everything else in the archive (README, licenses, `THIRD_PARTY_LICENSES`, `SBOM.cdx.json`, `THIRD_PARTY_SOURCES.tar.gz`, `vacancyd.example.toml`, these docs) is under `/usr/share/doc/vacancyd/`.

This means the container runs exactly the bytes that the release's checksums, notices and qualification describe, rather than a second build of the same version. The base is Chainguard's `glibc-dynamic` image, pinned by digest: glibc and the C runtime the binary links against, CA certificates, and no shell or package manager. Release qualification scans the exact platform images and refuses fixed high or critical findings before promotion.

| Path or setting | Value |
| --- | --- |
| Binary | `/usr/local/bin/vacancyd` (the entrypoint) |
| Default command | `run --config /etc/vacancyd/vacancyd.toml` |
| User | `65532:65532` |
| Configuration | `/etc/vacancyd/vacancyd.toml`, mounted read-only |
| State | `/var/lib/vacancyd` (owned by 65532, mode 0700); unfinished-work recovery records go to `admission/` there |
| API port | `9811`, or whatever `server.listen` sets |
| Proxy ports | whatever each `[[upstream]] listen` sets, for example `11435` for Ollama and `8189` for ComfyUI |
| Health check | `vacancyd healthcheck --config /etc/vacancyd/vacancyd.toml`, which requests `/healthz` |

The health check reports liveness only. A daemon that is running but refusing to lend (no GPU visible, presence unreadable) stays healthy and explains the refusal in `/status`, rather than being restarted in a loop. If you move the configuration file, override the health check with the same path.

## Headless: the default

`owner.presence = "headless"` tells vacancyd that nobody sits at this machine: it lends on capacity and the lending switches alone. That is the right mode for a container, which cannot see the host's desktop sessions without the extra access below.

A minimal configuration, lending one NVIDIA GPU to an Ollama container on the same Docker network:

```toml
[server]
listen = "0.0.0.0:9811"

[auth]
trust_loopback = true
allow_unauthenticated_remote = false

[[auth.token]]
name = "cluster"
scopes = ["borrow"]
value_file = "/run/secrets/vacancyd-borrow"

[[auth.token]]
name = "operator"
scopes = ["read", "admin"]
value_file = "/run/secrets/vacancyd-operator"

[owner]
presence = "headless"

[owner.gpu_pressure]
enabled = false

[[resource]]
id = "gpu0"
kind = "nvidia-gpu"
selector = "GPU-11111111-2222-3333-4444-555555555555"

[[upstream]]
id = "ollama"
listen = "0.0.0.0:11435"
target = "http://ollama:11434"
kind = "ollama"
resource = "gpu0"
default_min_free_mib = 8500
```

A target named by a service name, as here, is in another network namespace, so status reports its listener as `not_observable`: the container cannot see how the upstream is bound. Keep the upstream's port unpublished and check its bind on the Docker host.

Listeners must bind `0.0.0.0` inside the container for published ports to reach them, and a non-loopback listener requires tokens. Generate each one with `docker run --rm docker.io/probablyfinedev/vacancyd:vX.Y.Z gen-token` and store it in a file that only the container's user can read, for example `secrets/vacancyd-borrow` owned by 65532 with mode 0400. `value_file` follows the Docker secrets convention, so Compose secrets work unchanged. Keep `owner.gpu_pressure.enabled = false`; GPU attribution is not implemented on Linux. Measure model footprints as the README describes. Validate before starting:

```sh
docker run --rm -v "$PWD/vacancyd.toml:/etc/vacancyd/vacancyd.toml:ro" \
  -v "$PWD/secrets:/run/secrets:ro" \
  docker.io/probablyfinedev/vacancyd:vX.Y.Z check-config --config /etc/vacancyd/vacancyd.toml
```

With Compose:

```yaml
services:
  vacancyd:
    image: docker.io/probablyfinedev/vacancyd:vX.Y.Z
    restart: unless-stopped
    read_only: true
    tmpfs:
      - /run/vacancyd:uid=65532,gid=65532,mode=0755
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    ports:
      - "9811:9811"
      - "11435:11435"
    volumes:
      - ./vacancyd.toml:/etc/vacancyd/vacancyd.toml:ro
      - vacancyd-state:/var/lib/vacancyd
    secrets: [vacancyd-borrow, vacancyd-operator]
    gpus: all
  ollama:
    image: docker.io/ollama/ollama
    gpus: all
    # No published port: borrowers reach Ollama only through vacancyd.

secrets:
  vacancyd-borrow:
    file: ./secrets/vacancyd-borrow
  vacancyd-operator:
    file: ./secrets/vacancyd-operator

volumes:
  vacancyd-state:
```

Point borrowers at `http://<host>:11435` with the `borrow` token. Do not publish the inference server's own port: a request that reaches it directly bypasses vacancyd.

**State.** Keep `/var/lib/vacancyd` on a volume. Recovery records for work whose completion vacancyd could not confirm must survive a restart, or a restart would release that reservation. A new named volume takes the image's ownership (65532, mode 0700). A bind-mounted host directory must be owned by the user the container runs as: `sudo install -d -o 65532 -g 65532 -m 0700 /srv/vacancyd`.

**Loopback trust.** On a bridge network only the container itself is loopback, so `trust_loopback = true` covers the health check and `docker exec` and nothing else. Traffic through published ports arrives from the bridge gateway and needs a token. With `--network host`, every process on the host is loopback. As on any Linux install, a loopback client without a token may read status and settings and pause lending, and nothing else. See [authentication](authentication.md).

**Local administration.** Changes other than pausing need an `admin` token, as in the example above, or the admin socket inside the container, `/run/vacancyd/admin.sock`. The socket's directory must belong to the user the container runs as. The image ships it that way for 65532. With `--read-only`, mount a tmpfs there, with the uid and gid of `--user`: `--tmpfs /run/vacancyd:uid=65532,gid=65532,mode=0755`. Without a writable directory, the service logs once that the admin socket is unavailable and carries on; tokens and loopback pausing still work. The socket judges callers by their uid, as on the host. `docker exec` runs as the container's user, which may pause and resume its own pause. `docker exec -u 0` runs as root, which administers. Anyone who can run `docker exec` already controls the container. The request goes on standard input:

```sh
echo '{"protocol_version": 1, "action": {"kind": "set_resource", "resource": "gpu0", "enabled": false}}' |
  docker exec -i -u 0 vacancyd /usr/local/bin/vacancyd admin-request --request -
```

Keep the socket inside the container. Bind-mounting it to the host would let host accounts reach it. `admin-request` on the host would then refuse a server that runs as neither root nor the calling account, unless the socket's directory belongs to the container's user and nobody else can write to it. Settings cannot be saved in a container, because the configuration mount is read-only.

**Hardening.** The image needs no capabilities and writes only to `/var/lib/vacancyd` and the admin socket directory `/run/vacancyd`, so `--read-only` (with the tmpfs above), `--cap-drop ALL` and `--security-opt no-new-privileges` are expected to work, and the release qualification runs the image that way. Settings saved through the owner API fail against a read-only configuration mount; edit the file and restart the container instead.

## NVIDIA GPUs

vacancyd measures GPUs by running `nvidia-smi`. The image does not include it: the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) injects the host driver's `nvidia-smi` and its library when the container is started with GPU access:

```sh
docker run --gpus all ...            # or --gpus device=GPU-<uuid>
docker run --runtime nvidia -e NVIDIA_VISIBLE_DEVICES=all ...
```

The image sets `NVIDIA_DRIVER_CAPABILITIES=utility`, the capability that provides `nvidia-smi` and nothing for CUDA, because vacancyd only measures. Select GPUs by UUID in both the container's device list and each resource's `selector`. Without the toolkit, or with the wrong device list, the GPU's capacity is unreadable and every upstream that depends on it refuses with `503`; it never lends an unmeasured GPU. Check what the container sees with `docker run --rm --gpus all --entrypoint nvidia-smi docker.io/probablyfinedev/vacancyd:vX.Y.Z -L` (an entrypoint override works because the toolkit injects the binary).

## Desktop presence (logind)

The default `owner.presence = "auto"` reads systemd-logind over the host's system D-Bus, as the systemd service does. Without the access below, the container cannot reach logind and refuses to lend, logging `systemd-logind is unavailable`. It never lends because presence is unreadable.

Reading logind from a container needs the following. They were established on Ubuntu 24.04 (GNOME, dbus-daemon, AppArmor, Docker 29) and rechecked on Debian 13 (dbus-daemon, AppArmor, Docker 29):

1. **The bus socket.** Mount `/run/dbus/system_bus_socket` at the same path. The image sets `DBUS_SYSTEM_BUS_ADDRESS` to that path.
2. **A user the host knows.** dbus-daemon disconnects a client whose user ID has no entry in the host's user database, and the image's default 65532 normally has none. Run as `--user 65534:65534` (`nobody`, present on nearly every distribution) or as a dedicated system user you create on the host. Give that user its state directory: `sudo install -d -o 65534 -g 65534 -m 0700 /srv/vacancyd`.
3. **An AppArmor profile that allows the logind calls, on Ubuntu.** Ubuntu's dbus-daemon applies the container's AppArmor profile to its messages, and Docker's `docker-default` profile has no D-Bus rules, so the connection is refused at its first message. (Debian 13's dbus-daemon did not mediate, and `docker-default` worked there.) Where it is refused, load [`contrib/apparmor/vacancyd-logind`](../contrib/apparmor/vacancyd-logind), which is `docker-default` plus the three read-only calls vacancyd makes (`Hello`, `ListSessions`, and `GetAll` on a session), and select it:

   ```sh
   sudo install -m 0644 contrib/apparmor/vacancyd-logind /etc/apparmor.d/vacancyd-logind
   sudo apparmor_parser -r -W /etc/apparmor.d/vacancyd-logind
   ```

`--security-opt apparmor=unconfined` also works but removes the rest of the profile as well. The refusal names AppArmor in `presence.basis`.
4. **The host PID namespace, if you set `owner.processes`.** The owner-process list is matched against the processes of the signed-in user, which a container sees only with `--pid=host`. Without it vacancyd refuses to lend while a desktop is unlocked and explains that `owner.processes cannot be checked`; it does not treat a scan it cannot see as "no game running".

```sh
docker run -d --name vacancyd --network host \
  --user 65534:65534 --security-opt apparmor=vacancyd-logind \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /run/vacancyd:uid=65534,gid=65534,mode=0755 \
  -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket \
  -v /etc/vacancyd:/etc/vacancyd:ro -v /srv/vacancyd:/var/lib/vacancyd \
  --gpus all docker.io/probablyfinedev/vacancyd:vX.Y.Z
```

Add `--pid=host` only when `owner.processes` is set. Confirm the result in `/status`: `presence_backend` is `logind`, `presence.sessions` lists the host's sessions, and the basis does not say logind is unavailable.

Verified in that environment: the sign-in screen alone lends, an unlocked GNOME Wayland session refuses, a locked one lends, and with `--pid=host` a listed process of the signed-in user refuses while the session is unlocked and lending resumes when it exits. Not verified: rootless Docker and Podman, Docker with user-namespace remapping (both change the user ID the host's D-Bus sees), hosts running dbus-broker instead of dbus-daemon, and SELinux hosts, which confine the socket separately. On those, use `headless` or the systemd service.

## How the image is built

Releases are built by the maintainer's private release pipeline from the tagged public source ([releasing](releasing.md#how-a-release-is-built)). Before it builds anything for a release, the pipeline proves that the public tag holds exactly the source it is building. Then:

1. It builds the two Linux release archives, and assembles the image from those same archives with `packaging/container/Dockerfile`: one OCI archive holding both platforms, with reproducible timestamps taken from the tagged commit. `scripts/container_release.py inputs` stages the archives and checks each against its sidecar.
2. It runs each platform's image natively, on an amd64 and an arm64 host, with `scripts/qualify_container.py`, and scans each with Trivy. A fixed high or critical vulnerability stops the release. Trivy also writes a CycloneDX SBOM for each platform.
3. Assembly opens the OCI archive itself rather than trusting the job that built it (`scripts/container_release.py verify`). Each platform's image must be the pinned base image, read from its registry by digest, plus one layer holding exactly the files of that platform's release archive with the documented owners and modes, and must have the configuration and labels the Dockerfile declares. Both platforms must have passed, and each qualification, scan and SBOM must name that exact image.
4. Publication copies that exact OCI archive to Docker Hub and GHCR with `scripts/container_release.py publish` (`skopeo --preserve-digests`), so both registries serve the digest that was tested. It creates the immutable `vX.Y.Z` tag, refuses to overwrite one that holds other content, and reads every tag back. Nothing is rebuilt after qualification.
5. It signs the image index in both registries with the project's release key, and attaches signed attestations: SLSA provenance on the index, naming the tag's commit and the two archives by digest, and each platform's SBOM on that platform's manifest.

## Checking an image

The release key's public half is [`packaging/cosign.pub`](../packaging/cosign.pub) in the repository. With [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) installed:

```sh
cosign verify --key cosign.pub docker.io/probablyfinedev/vacancyd:vX.Y.Z
cosign verify-attestation --key cosign.pub --type slsaprovenance1 \
  docker.io/probablyfinedev/vacancyd:vX.Y.Z
```

The same commands work for `ghcr.io/maxfield-allison/vacancyd`. The signatures are recorded in Sigstore's public transparency log, which cosign checks by default. To read a platform's SBOM, verify the attestation on that platform's manifest digest, which `docker buildx imagetools inspect` lists:

```sh
cosign verify-attestation --key cosign.pub --type cyclonedx \
  docker.io/probablyfinedev/vacancyd@sha256:<platform manifest digest>
```

The release notes name the image index digest, and the release's signed `SHA256SUMS` covers the Linux archives the image was assembled from.

## Building it yourself

Build the Linux archive as `docs/releasing.md` describes, or download one from a release, then pass the directory holding the archive and its `.sha256` sidecar as the build context:

```sh
docker buildx build -f packaging/container/Dockerfile \
  --build-arg VERSION=X.Y.Z --platform linux/amd64 -t vacancyd:local dist/
python3 scripts/qualify_container.py --image vacancyd:local --version X.Y.Z --evidence target/qualification
```

`qualify_container.py` needs only the Docker CLI and a Python image to pull. It runs the image read-only with no capabilities against a stub upstream and checks the user, health check, token scopes, a served borrow request, the lending switch, a graceful stop, and the refusals for unreadable presence and an unreadable GPU. On a host with an NVIDIA GPU and the toolkit, add `--gpu all` to lend the real GPU instead of memory.
