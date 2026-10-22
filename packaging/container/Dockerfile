# vacancyd container image, assembled from the Linux release archives.
#
# The image does not compile vacancyd. It unpacks the published release archive
# for its architecture, so the binary inside is byte for byte the one in
# vacancyd-<version>-<target>.tar.gz and covered by that archive's checksum,
# notices and qualification. See docs/container.md.
#
# Build context: a directory holding the archive(s) and .sha256 sidecar(s).
#
#   docker buildx build -f packaging/container/Dockerfile \
#     --build-arg VERSION=1.0.0 --platform linux/amd64 dist/
#
# Every stage that runs a command runs on the build host, so building the
# arm64 image needs no emulation. Base images are pinned by digest; move a
# pin deliberately and re-run the container qualification.

ARG TOOLS_IMAGE=docker.io/library/busybox:1.37.0@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
# glibc and the C runtime the binary links against, CA certificates, no shell.
# Chosen over distroless cc-debian12/13, which carried unfixed high and critical
# glibc, OpenSSL and zlib findings when this was pinned (docs/container.md).
ARG RUNTIME_IMAGE=cgr.dev/chainguard/glibc-dynamic:latest@sha256:3f1283dc2a6a48c25cd2e1c7730a871b19b721975240e108e0d6a73b555a3a20

FROM --platform=$BUILDPLATFORM ${TOOLS_IMAGE} AS payload
ARG TARGETARCH
ARG VERSION
WORKDIR /archives
COPY vacancyd-*-unknown-linux-gnu.tar.gz vacancyd-*-unknown-linux-gnu.tar.gz.sha256 ./
RUN set -eu; \
    : "${VERSION:?pass --build-arg VERSION=X.Y.Z (the archive version, without v)}"; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "no vacancyd release archive for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    stem="vacancyd-$VERSION-$target"; \
    sha256sum -c "$stem.tar.gz.sha256"; \
    mkdir /unpacked; \
    tar -xzf "$stem.tar.gz" -C /unpacked; \
    test "$(ls /unpacked)" = "$stem"; \
    test -f "/unpacked/$stem/vacancyd"; \
    mkdir -p /out/usr/local/bin /out/usr/share/doc /out/etc/vacancyd /out/var/lib/vacancyd /out/run/vacancyd; \
    mv "/unpacked/$stem/vacancyd" /out/usr/local/bin/vacancyd; \
    mv "/unpacked/$stem" /out/usr/share/doc/vacancyd; \
    chown -R 0:0 /out; \
    chmod -R u=rwX,go=rX /out; \
    chmod 0755 /out/usr/local/bin/vacancyd; \
    chown 65532:65532 /out/var/lib/vacancyd /out/run/vacancyd; \
    chmod 0700 /out/var/lib/vacancyd; \
    chmod 0755 /out/run/vacancyd

FROM ${RUNTIME_IMAGE}
ARG RUNTIME_IMAGE
ARG VERSION
ARG REVISION=unknown
ARG CREATED=1980-01-01T00:00:00Z
LABEL org.opencontainers.image.title="vacancyd" \
      org.opencontainers.image.description="Gate borrowed AI requests on owner policy and available resources." \
      org.opencontainers.image.source="https://github.com/maxfield-allison/vacancyd" \
      org.opencontainers.image.url="https://github.com/maxfield-allison/vacancyd" \
      org.opencontainers.image.documentation="https://github.com/maxfield-allison/vacancyd/blob/main/docs/container.md" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="maxfield-allison" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="${RUNTIME_IMAGE}"

COPY --from=payload /out/ /

# Unset admission_state_dir resolves to $STATE_DIRECTORY/admission, which keeps
# recovery records off the read-only configuration mount. vacancyd only runs
# nvidia-smi, so the NVIDIA Container Toolkit needs to inject the utility
# driver capability and nothing for CUDA. The bus address names the path the
# logind mode mounts; without it the D-Bus client looks under /var/run, which
# a minimal base need not link to /run.
ENV STATE_DIRECTORY=/var/lib/vacancyd \
    NVIDIA_DRIVER_CAPABILITIES=utility \
    DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket

# The base image's "nonroot" user; numeric so runtimes can verify runAsNonRoot.
USER 65532:65532

# The API listener. Proxy ports are whatever [[upstream]].listen configures.
EXPOSE 9811

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=2s --retries=3 \
    CMD ["/usr/local/bin/vacancyd", "healthcheck", "--config", "/etc/vacancyd/vacancyd.toml"]

ENTRYPOINT ["/usr/local/bin/vacancyd"]
CMD ["run", "--config", "/etc/vacancyd/vacancyd.toml"]
