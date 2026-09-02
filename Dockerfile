# syntax=docker/dockerfile:1.7
#
# Multi-stage Dockerfile for MCP Agent Mail (Rust).
#
# Mirrors the sibling-clone build pattern from .github/workflows/dist.yml.
# The current workspace has exactly two out-of-tree path authorities:
# frankensearch through workspace dependencies and beads_rust through
# `[patch.crates-io]`. The build clones both as siblings of
# /build/mcp_agent_mail_rust so Cargo resolves those paths correctly.
#
# Build arguments
# ---------------
#   AM_REF       Git ref of mcp_agent_mail_rust to build (default: main).
#                Pass a tag, branch, or commit SHA.
#   SIBLING_REF  Preferred Git ref for the beads_rust sibling (default: main).
#                Release builds pass the release tag here first, so a sibling
#                repo with a matching tag is pinned instead of floating.
#   SIBLING_FALLBACK_REF
#                Fallback ref when SIBLING_REF is a branch/tag that a sibling
#                repo does not publish yet (default: main).
#   FRANKENSEARCH_COMMIT
#                Immutable frankensearch revision this workspace builds
#                against. It is NOT a floating sibling: the live frankensearch
#                tree already moved to an asupersync the rest of the workspace
#                cannot follow, so its default here must stay identical to
#                `FRANKENSEARCH_COMMIT` in .github/workflows/dist.yml
#                (tests/docs_drift_ci.rs pins the two together).
#
# Examples
# --------
#   # Build the current tip of main:
#   docker build -t mcp-agent-mail-rust:dev .
#
#   # Build a tagged release:
#   docker build --build-arg AM_REF=v0.3.6 -t mcp-agent-mail-rust:v0.3.6 .
#
#   # Build a specific commit (e.g. between releases):
#   docker build --build-arg AM_REF=abc1234 -t mcp-agent-mail-rust:dev .
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
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --no-modify-path \
    && chmod -R a+rwX "$RUSTUP_HOME" "$CARGO_HOME"

WORKDIR /build

ARG AM_REF=main
ARG SIBLING_REF=main
ARG SIBLING_FALLBACK_REF=main
# Keep in lock-step with FRANKENSEARCH_COMMIT in .github/workflows/dist.yml.
ARG FRANKENSEARCH_COMMIT=3bbfd8c664062f8304e7a790c51794671f9214dc

# Clone the sibling checkouts that /Cargo.toml still resolves through a `path`,
# so they're cached separately from the project source layer.
#
# GH#256: this list must track the out-of-tree `path` entries in /Cargo.toml and
# nothing else. It used to clone ten repos, which is how the image went stale at
# v0.3.13 — most of that universe moved to crates.io (asupersync, the sqlmodel
# and fsqlite families, fastmcp, ftui, franken-agent-detection, tru, rich_rust),
# but the clone list kept trying to satisfy patches that no longer existed while
# the two that *do* still need a sibling failed for unrelated reasons. As of the
# 2026-08-22 registry adoption exactly two remain:
#
#   frankensearch  — [workspace.dependencies] path = "../frankensearch-rel-0332/..."
#                    (gated clone name; the pinned commit is cloned under it)
#   beads_rust     — [patch.crates-io] path = "../beads_rust"
#                    (registry 0.3.2 pins asupersync =0.4.4, unsatisfiable in
#                     the =0.4.9 universe; drops out when 0.4.x publishes)
#
# The `sibling path deps` guard below fails the build the next time that
# invariant breaks, naming the sibling, instead of letting cargo fail late with
# a bare "No such file or directory (os error 2)".
#
# `git clone --depth 1 --branch <ref>` only accepts branch/tag names — passing
# a commit SHA fails with "Remote branch <sha> not found in upstream origin".
# `clone_at` detects full 40-char SHAs and falls back to init+fetch+checkout.
# GitHub's smart-http upload-pack honors full SHAs (via
# uploadpack.allowReachableSHA1InWant) but NOT abbreviated/short SHAs —
# `git fetch origin abc1234` errors out with "couldn't find remote ref".
# Pass the full 40-char SHA, a branch, or a tag; short SHAs are rejected.
#
# `--sparse` is a clone-only flag; the SHA path explicitly initializes
# sparse-checkout before the fetch instead of passing it through `git fetch`
# (which doesn't accept `--sparse`).
#
# `set -eu` (no `x`) keeps logs readable; per-clone `echo` provides progress.
# Any failure aborts the build immediately rather than silently dropping a
# dependency (same guarantee as dist.yml).
RUN set -eu; \
    clone_at() { \
      url="$1"; ref="$2"; dest="$3"; shift 3; \
      echo "+ clone_at $url @ $ref -> $dest $*"; \
      sparse=0; \
      pass_args=""; \
      for arg in "$@"; do \
        case "$arg" in \
          --sparse) sparse=1 ;; \
          *) pass_args="$pass_args $arg" ;; \
        esac; \
      done; \
      if printf '%s' "$ref" | grep -Eq '^[0-9a-f]{40}$'; then \
        git init -q "$dest"; \
        ( cd "$dest" \
          && git remote add origin "$url" \
          && if [ "$sparse" = 1 ]; then git sparse-checkout init --no-cone; fi \
          && git fetch --depth 1 $pass_args origin "$ref" \
          && git checkout -q FETCH_HEAD ); \
      else \
        selected_ref="$ref"; \
        if ! git ls-remote --exit-code "$url" "refs/heads/$ref" "refs/tags/$ref" >/dev/null 2>&1; then \
          if [ -n "${SIBLING_FALLBACK_REF:-}" ] && [ "$ref" != "$SIBLING_FALLBACK_REF" ]; then \
            echo "+ sibling ref $ref not found for $url; falling back to $SIBLING_FALLBACK_REF"; \
            selected_ref="$SIBLING_FALLBACK_REF"; \
          else \
            echo "sibling ref $ref not found for $url and no fallback is available" >&2; \
            exit 1; \
          fi; \
        fi; \
        git clone --depth 1 --branch "$selected_ref" "$@" "$url" "$dest"; \
      fi; \
    }; \
    clone_at https://github.com/Dicklesworthstone/frankensearch.git "${FRANKENSEARCH_COMMIT}" /build/frankensearch-rel-0332; \
    clone_at https://github.com/Dicklesworthstone/beads_rust.git    "${SIBLING_REF}" /build/beads_rust

# Clone the project source at the requested ref. We clone (rather than COPY
# the build context) so the image is reproducible from `docker build .` with
# no local working tree: passing AM_REF=v0.3.6 produces the exact same image
# from any checkout. CI passes ${{ github.sha }} for tagged releases.
#
# Same SHA-vs-ref handling as the sibling clones above — see the `clone_at`
# helper for why `--branch <sha>` doesn't work and what we do instead.
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

# GH#256 drift guard: every out-of-tree `path` in /Cargo.toml must have been
# cloned above. Without this the failure mode is cargo dying deep into the build
# with "failed to load source for dependency <x>: No such file or directory
# (os error 2)", which is what kept the published image at v0.3.13 across five
# tagged releases. Fail here instead, naming the missing sibling and the line
# that wants it, so the fix is one clone_at away.
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
      echo "Add a matching clone_at line to the sibling-clone step above, or move the" >&2; \
      echo "dependency to crates.io. See GH#256." >&2; \
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
    cargo build --release \
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
