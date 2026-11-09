# syntax=docker/dockerfile:1.7
#
# Multi-stage Dockerfile for MCP Agent Mail (Rust).
#
# Cargo.lock and immutable Git revisions select the dependency sources.
# No sibling checkout is required by the current workspace.
#
# Build arguments
# ---------------
#   AM_REF       Git ref of mcp_agent_mail_rust to build (default: main).
#                Pass a tag, branch, or commit SHA.
#   FRANKENSEARCH_COMMIT
#                Immutable frankensearch revision this workspace builds
#                against. Cargo and container builds must use the same
#                reviewed source, so its default here must stay identical to
#                `FRANKENSEARCH_COMMIT` in .github/workflows/dist.yml
#                (tests/docs_drift_ci.rs pins the two together).
#
# Examples
# --------
#   # Build the current tip of main:
#   docker build -t mcp-agent-mail-rust:dev .
#
#   # Build the exact current source commit:
#   docker build --build-arg AM_REF="$(git rev-parse HEAD)" -t mcp-agent-mail-rust:dev .
#
# For another ref, pass its matching FRANKENSEARCH_COMMIT from Cargo.toml.
#
# Runtime
# -------
# The image ships two binaries that share a /data volume:
#   * mcp-agent-mail (the server, ENTRYPOINT)
#   * am             (the operator CLI)
#
# Default CMD is `serve --no-tui` — containers are headless. To get an
# interactive operator console, override CMD with `am tui` and run with
# `-it -e TUI_ENABLED=true`.

# ─── Stage 1: builder ────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS builder

# rustup is installed instead of FROM rust:<version>-slim because this
# workspace pins a specific nightly via rust-toolchain.toml; rustup will
# auto-install the pinned channel on first cargo invocation. This keeps the
# Dockerfile in lock-step with rust-toolchain.toml without manual edits.
ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        pkg-config \
        libsqlite3-dev \
        build-essential \
        cmake \
        clang \
        libssl-dev \
        python3 \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf \
    --user-agent 'OpenAI File Downloader, XaiImageApiFetch/1.0' https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --no-modify-path \
    && chmod -R a+rwX "$RUSTUP_HOME" "$CARGO_HOME"

WORKDIR /build

ARG AM_REF=main
# Keep in lock-step with FRANKENSEARCH_COMMIT in .github/workflows/dist.yml.
ARG FRANKENSEARCH_COMMIT=616c9a7a6bdada97d81f760808996911ba3be294

# Clone the requested source independently of the local working tree. Full
# commit SHAs pin the project source; Cargo.lock binds dependency selection.
# Git's --branch accepts branches/tags, so full SHAs use fetch + checkout.
RUN set -eu; \
    if printf '%s' "${AM_REF}" | grep -Eq '^[0-9a-f]{40}$'; then \
      git init -q /build/mcp_agent_mail_rust; \
      cd /build/mcp_agent_mail_rust; \
      git remote add origin https://github.com/Dicklesworthstone/mcp_agent_mail_rust.git; \
      git fetch --depth 1 origin "${AM_REF}"; \
      git checkout -q FETCH_HEAD; \
    else \
      git clone --depth 1 --branch "${AM_REF}" \
        https://github.com/Dicklesworthstone/mcp_agent_mail_rust.git \
        /build/mcp_agent_mail_rust; \
    fi

WORKDIR /build/mcp_agent_mail_rust

# The declared container pin must identify the dependencies Cargo will use.
# An unused sibling clone cannot establish that agreement.
RUN python3 - "$FRANKENSEARCH_COMMIT" <<'PY'
import pathlib
import re
import sys
import tomllib

pin = sys.argv[1]
if not re.fullmatch(r"[0-9a-f]{40}", pin):
    sys.exit("FRANKENSEARCH_COMMIT must be one full lowercase Git SHA")
repo = "https://github.com/Dicklesworthstone/frankensearch"
manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text())
lock = tomllib.loads(pathlib.Path("Cargo.lock").read_text())
dependencies = {
    name: value for name, value in manifest["workspace"]["dependencies"].items()
    if name == "frankensearch" or name.startswith("frankensearch-")
}
if "frankensearch" not in dependencies:
    sys.exit("Cargo.toml has no frankensearch dependency to validate")
source = f"git+{repo}?rev={pin}#{pin}"
for name, dependency in dependencies.items():
    if (not isinstance(dependency, dict) or dependency.get("git") != repo
            or dependency.get("rev") != pin or "path" in dependency):
        sys.exit(f"Cargo.toml {name} does not use FRANKENSEARCH_COMMIT")
    package = dependency.get("package", name)
    matches = [entry for entry in lock["package"] if entry["name"] == package]
    if len(matches) != 1 or matches[0].get("source") != source:
        sys.exit(f"Cargo.lock {package} does not use FRANKENSEARCH_COMMIT")
for entry in lock["package"]:
    if (entry["name"] == "frankensearch" or entry["name"].startswith("frankensearch-")) and entry.get("source") != source:
        sys.exit(f"Cargo.lock {entry['name']} uses another frankensearch revision")
print(f"+ Cargo.toml and Cargo.lock agree on frankensearch {pin}")
PY

# GH#256 drift guard: reject missing out-of-tree paths before Cargo starts.
# Otherwise the failure mode is cargo dying deep into the build
# with "failed to load source for dependency <x>: No such file or directory
# (os error 2)", which is what kept the published image at v0.3.13 across five
# tagged releases. Fail here instead, naming the missing sibling and the line
# that wants it. Dependencies must resolve within this checkout or through Cargo.
#
# Matches `path = "../..."` anywhere in the manifest (both
# [workspace.dependencies] and [patch.crates-io] use the same spelling), then
# resolves it against this WORKDIR exactly as cargo would.
RUN set -eu; \
    missing="$( \
      grep -oE 'path *= *"\.\./[^"]*"' Cargo.toml \
        | sed -E 's/.*"(.*)"/\1/' \
        | sort -u \
        | while IFS= read -r rel; do \
            if [ -d "$rel" ]; then \
              echo "+ sibling path dep present: $rel" >&2; \
            else \
              echo "$rel"; \
            fi; \
          done \
    )"; \
    if [ -n "$missing" ]; then \
      echo "" >&2; \
      echo "Cargo.toml declares out-of-tree path dependencies this image does not clone:" >&2; \
      printf '%s\n' "$missing" | sed 's/^/  - /' >&2; \
      echo "" >&2; \
      echo "Use a registry dependency or an immutable Cargo Git revision. See GH#256." >&2; \
      exit 1; \
    fi; \
    echo "+ every out-of-tree path dependency is present"

# Pre-install the pinned nightly toolchain so the build step's diagnostics
# don't interleave a toolchain download with cargo output. `rustup show` is
# the documented incantation that triggers rust-toolchain.toml resolution.
RUN rustup show active-toolchain || rustup show

# Mirrors the release flags in dist.yml exactly:
#   --no-default-features --features portable excludes the hybrid/fastembed
#   ONNX runtime (its AVX2-only static libs SIGILL on older x86 CPUs).
#   Portable retains s3fifo + tantivy-engine.
#
# strip is applied here (in addition to the workspace `strip = "symbols"`
# release profile) for belt-and-braces — keeps the runtime image small even
# if the profile is ever relaxed.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/mcp_agent_mail_rust/target,sharing=locked \
    cargo build --release --locked \
        -p mcp-agent-mail -p mcp-agent-mail-cli \
        --no-default-features --features portable \
    && mkdir -p /out \
    && cp target/release/mcp-agent-mail /out/mcp-agent-mail \
    && cp target/release/am             /out/am \
    && strip /out/mcp-agent-mail /out/am

# Surface verification: refuse to ship if the two binaries got swapped or
# either is missing its expected CLI surface. Same checks as dist.yml.
RUN /out/mcp-agent-mail --help > /tmp/server.help \
    && /out/am --help          > /tmp/cli.help \
    && grep -qE '^Usage: mcp-agent-mail ' /tmp/server.help \
    && grep -qE '(^|[[:space:]])serve([[:space:]]|$)' /tmp/server.help \
    && grep -qE '(^|[[:space:]])serve-http([[:space:]]|$)' /tmp/cli.help

# ─── Stage 2: runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# Runtime needs:
#   * ca-certificates — outbound HTTPS (e.g. share-link uploads)
#   * curl            — HEALTHCHECK
#   * git             — `am` shells out for repo introspection
#   * libsqlite3-0    — frankensqlite ships its own engine but SQLite system
#                       libs are needed for some integration paths
#   * tini            — PID 1 reaper so signal handling and zombie reaping
#                       work correctly when running under Docker/Kubernetes
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        libsqlite3-0 \
        tini \
    && rm -rf /var/lib/apt/lists/* \
    && adduser --system --group --home /home/appuser --uid 10001 appuser \
    && mkdir -p /data/mailbox \
    && chown -R appuser:appuser /data /home/appuser \
    # Bind-mounted /data volumes inherit the host's uid/gid, which is rarely
    # 10001:10001 — without `safe.directory` git refuses to run with "dubious
    # ownership" the first time `am` touches a project's git repo. Configure
    # both system-wide and per-user so it works regardless of who runs git.
    && git config --system --add safe.directory '*' \
    && install -d -o appuser -g appuser /home/appuser/.config/git \
    && printf '[safe]\n\tdirectory = *\n' \
        > /home/appuser/.config/git/config \
    && chown appuser:appuser /home/appuser/.config/git/config

COPY --from=builder /out/mcp-agent-mail /usr/local/bin/mcp-agent-mail
COPY --from=builder /out/am             /usr/local/bin/am

# Environment defaults
#   HTTP_HOST=0.0.0.0  — bind all interfaces (the in-container default of
#                        127.0.0.1 is unreachable from outside).
#   HTTP_PORT=8765     — matches dist.yml + Python image conventions.
#   HTTP_PATH=/mcp/    — server default; surfaced here for visibility.
#   STORAGE_ROOT=/data/mailbox — single mounted volume keeps DB + archives
#                                co-located for backups.
#   TUI_ENABLED=false  — containers are headless. Override + run with -it
#                        to use the operator console.
#   AM_INTERFACE_MODE=mcp — explicit default (matches code default).
ENV HTTP_HOST=0.0.0.0 \
    HTTP_PORT=8765 \
    HTTP_PATH=/mcp/ \
    STORAGE_ROOT=/data/mailbox \
    TUI_ENABLED=false \
    AM_INTERFACE_MODE=mcp \
    RUST_LOG=info

EXPOSE 8765
VOLUME ["/data"]

USER appuser
WORKDIR /home/appuser

# Liveness probe hits the unauthenticated health endpoint. The 20s start
# period + 5 retries (matching the Python sibling image's HEALTHCHECK) keeps
# Docker from flapping the container during the initial DB migration on first
# boot, especially on slow ARM hosts or under QEMU emulation where the Rust
# binary's startup can take noticeably longer than on amd64.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=5 \
    CMD curl -fsS "http://127.0.0.1:${HTTP_PORT}/health/liveness" || exit 1

# tini handles signal forwarding + zombie reaping. The server itself ignores
# SIGPIPE by default, but tini ensures SIGTERM from `docker stop` reaches it
# promptly without the 10s grace timeout.
ENTRYPOINT ["/usr/bin/tini", "--", "mcp-agent-mail"]
CMD ["serve", "--no-tui"]

# OCI labels — discoverable via `docker inspect` and shown on the GHCR page.
LABEL org.opencontainers.image.title="MCP Agent Mail (Rust)" \
      org.opencontainers.image.description="Multi-agent coordination via MCP — Rust rewrite. Ships mcp-agent-mail (server) and am (operator CLI)." \
      org.opencontainers.image.source="https://github.com/Dicklesworthstone/mcp_agent_mail_rust" \
      org.opencontainers.image.licenses="LicenseRef-MIT-Rider" \
      org.opencontainers.image.vendor="Jeff Emanuel"
