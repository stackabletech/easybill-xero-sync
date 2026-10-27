# syntax=docker/dockerfile:1.16.0@sha256:e2dd261f92e4b763d789984f6eab84be66ab4f5f08052316d8eb8f173593acf7
# check=error=true;skip=SecretsUsedInArgOrEnv
# SecretsUsedInArgOrEnv only matches names: XERO_AUTH is the auth *mode*
# (oauth / client-credentials), not a secret. Real secrets come in at runtime.

#
# Two stages on Red Hat Project Hummingbird images:
#
# - builder: hi/rust (builder variant) compiles a fully static binary (glibc,
#   `crt-static`), so the runtime needs no C library at all.
# - final: hi/static, which only holds CA certificates, timezone data and a
#   non-root user (65532). No shell, no package manager, no libc.
#
# DNS works without NSS modules: static glibc (>= 2.34) has the `files` and
# `dns` lookups built in and reads /etc/hosts and /etc/resolv.conf. TLS roots
# are compiled into the binary (webpki-roots).
#

# Find the latest version:
#
#   docker buildx imagetools inspect registry.access.redhat.com/hi/rust:1-builder --format '{{json .Manifest.Digest}}'
#
FROM registry.access.redhat.com/hi/rust:1.99.1-builder@sha256:d8d6769b8622d051d82b194dee469f4feaf102aa6d9e6a7d86f7de3b1740f2be AS builder

# Find the latest version here: https://crates.io/crates/cargo-auditable
# renovate: datasource=crate packageName=cargo-auditable
ENV CARGO_AUDITABLE_CRATE_VERSION=0.7.7

# Sets the default shell to Bash with strict error handling and robust pipeline processing.
# "-e": Exits immediately if a command exits with a non-zero status
# "-u": Treats unset variables as an error, preventing unexpected behavior from undefined variables.
# "-o pipefail": Causes a pipeline to return the exit status of the last command in the pipe that failed, ensuring errors in any part of a pipeline are not ignored.
# "-c": Allows the execution of commands passed as a string
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

RUN <<EOF
# glibc-static: the static C library, needed to link with `crt-static`.
dnf install --assumeyes --setopt=install_weak_deps=False glibc-static
dnf clean all

# cargo-auditable embeds the dependency list into the binary, so scanners
# (grype, trivy) can see the Rust dependencies of the otherwise opaque binary.
cargo install --locked "cargo-auditable@${CARGO_AUDITABLE_CRATE_VERSION}"
EOF

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY easybill/ easybill/
COPY src/ src/

# An explicit `--target` keeps `crt-static` off the build scripts and proc
# macros, which must stay dynamic.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    <<EOF
target="$(rustc -vV | sed -n 's/^host: //p')"
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo auditable build --release --locked --target "${target}"

binary="target/${target}/release/easybill-xero"
# Fail the build if anything is still linked dynamically.
if readelf -d "${binary}" | grep -q NEEDED; then
  echo "${binary} is not static:" >&2
  readelf -d "${binary}" | grep NEEDED >&2
  exit 1
fi
install -D -m 0755 "${binary}" /out/easybill-xero
EOF

# Find the latest version:
#
#   docker buildx imagetools inspect registry.access.redhat.com/hi/static:latest --format '{{json .Manifest.Digest}}'
#
FROM registry.access.redhat.com/hi/static:latest@sha256:c51e17de4a96e3bd68361830ec54f3c43ffcf398b82765a1ce250bd4a0d99d1f AS final

ARG VERSION=dev
ARG TARGETARCH

LABEL name="easybill-xero"
LABEL summary="One-way sync from easybill into Xero"
LABEL description="Pushes easybill customers, invoices and credit notes into a Xero organisation."
LABEL version="${VERSION}"
LABEL maintainer="info@stackable.tech"
LABEL vendor="Stackable GmbH"
LABEL url="https://stackable.tech"
LABEL architecture="${TARGETARCH}"
# Clear labels inherited from the Hummingbird base that would describe it, not this image.
LABEL io.k8s.description="Pushes easybill customers, invoices and credit notes into a Xero organisation."
LABEL io.hummingbird-project.major-minor-version=""
LABEL io.hummingbird-project.major-version=""
LABEL io.hummingbird-project.variant.description=""
LABEL org.opencontainers.image.title="easybill-xero"
LABEL org.opencontainers.image.description="Pushes easybill customers, invoices and credit notes into a Xero organisation."
LABEL org.opencontainers.image.version="${VERSION}"
LABEL org.opencontainers.image.authors="info@stackable.tech"
LABEL org.opencontainers.image.url="https://stackable.tech"
LABEL org.opencontainers.image.vendor="Stackable GmbH"
LABEL org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /out/easybill-xero /usr/local/bin/easybill-xero

# Production defaults: custom connection, config and state on mounted volumes,
# JSON logs. Each can be overridden with the matching environment variable.
ENV XERO_AUTH=client-credentials \
    EASYBILL_XERO_CONFIG=/config/config.toml \
    EASYBILL_XERO_STATE_FILE=/state/state.json \
    EASYBILL_XERO_LOG_FORMAT=json

# The base image's non-root user; /state must be writable for it (e.g. a PVC
# with `fsGroup`).
USER 65532

ENTRYPOINT ["/usr/local/bin/easybill-xero"]
CMD ["--help"]
