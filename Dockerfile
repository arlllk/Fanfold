# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
#
# fanfold: the service, plus the pinned FanFicFare it runs as a subprocess.
#
#   docker build -t fanfold .
#   docker run --rm -v ./config.toml:/etc/fanfold/config.toml:ro fanfold check-config
#
# Configuration is a file (default /etc/fanfold/config.toml, or $FANFOLD_CONFIG); secrets come
# from environment variables or files the config points at. Nothing secret is baked in.
#
# RENOVATE. Every version below is kept current by Renovate (see renovate.json): base images
# are written out in full with their digest on the FROM line, the two tool versions carry a
# `# renovate:` comment naming their datasource, and FanFicFare with everything under it is
# locked, with hashes, in python/uv.lock. Keep that shape when editing this file.
#
# REPRODUCIBLE. The same commit builds the same image, byte for byte (same digest): every input
# is pinned (base images by digest, crates by Cargo.lock, Python packages by hash, wheels only),
# nothing is fetched from apt, and the timestamps come from SOURCE_DATE_EPOCH (the commit time)
# with BuildKit's `rewrite-timestamp=true`. CI passes both; by hand:
#
#   SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) docker buildx build \
#     --output type=docker,rewrite-timestamp=true -t fanfold .
#
# CACHING. The layers are ordered so that the slow steps rarely re-run: the dependency build
# (cargo-chef) is keyed on the dependency graph only, so a source change recompiles our own
# crates and nothing else. CI exports all of these layers to the GitHub Actions cache.
#
# LICENCES. The image carries Fanfold's licence texts and a generated notice for every Rust crate
# and Python package in it, in /usr/share/doc/fanfold. The build fails if a dependency has a
# licence that is not on the allowlists in licenses/ (copyleft needs to be allowed by name).
#
# HARDENING. The image runs as a non-root user, needs no capabilities, and works with a
# read-only root filesystem: the only places the service writes are /library, /tmp/fanfold and
# /run/fanfold, and each can be a volume or an empty tmpfs. It also runs under rootless
# Docker/Podman and with an arbitrary --user (see APP_UID/APP_GID, and the README).
#
#   docker run --read-only --cap-drop ALL --security-opt no-new-privileges \
#     --tmpfs /tmp/fanfold:rw,nosuid,nodev,mode=0770,uid=10001,gid=10001 \
#     --tmpfs /run/fanfold:rw,nosuid,nodev,mode=0700,uid=10001,gid=10001 \
#     -v library:/library -v ./config.toml:/etc/fanfold/config.toml:ro fanfold

# ------------------------------------------------------------------------ python base
# One definition of the Python base, shared by the venv and the runtime stage: the venv's
# interpreter is a link into this image, so the two must be the very same image.
FROM python:3.15-slim-trixie@sha256:65a65fc7b30b06cb6df4930fd4650697cb23f4c2698ab81fc87e4b031bc36928 AS python-base

# ----------------------------------------------------------------------------- chef
FROM rust:1.100.0-trixie@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS chef
# renovate: datasource=crate depName=cargo-chef
ARG CARGO_CHEF_VERSION=0.1.78
RUN cargo install cargo-chef --version "${CARGO_CHEF_VERSION}" --locked
WORKDIR /src

# ------------------------------------------------------------------------ cargo-about
# Generates the licence notice for the Rust dependencies, and fails the build on a licence that
# is not allowed (licenses/about.toml). A stage of its own, so installing it is cached
# independently of everything else.
FROM chef AS about-tool
# renovate: datasource=crate depName=cargo-about
ARG CARGO_ABOUT_VERSION=0.9.3
# `--features cli` is required: without it cargo builds the library only and installs nothing,
# yet exits 0. The version check makes a missing binary fail the build.
RUN cargo install cargo-about --version "${CARGO_ABOUT_VERSION}" --locked --features cli \
 && cargo about --version

# --------------------------------------------------------------------------- planner
# The dependency graph, as one small file. Everything that is not a manifest is dropped, so
# the next stage's cache key does not move when only source code changes.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY fuzz ./fuzz
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------- deps
FROM chef AS deps
COPY --from=planner /src/recipe.json recipe.json
# Dependencies only: this layer is rebuilt when Cargo.lock or a manifest changes, not before.
# (cargo-chef leaves the dependency skeleton, and the downloaded crates, in place for the
# stages below.)
RUN cargo chef cook --release --locked --package fanfold-server --recipe-path recipe.json

# ---------------------------------------------------------------------------- build
FROM deps AS build
# Our own code. (rust-toolchain.toml is deliberately not copied: the image's toolchain is the
# pinned one, and rustup must not fetch another.)
COPY Cargo.toml Cargo.lock clippy.toml rustfmt.toml ./
COPY crates ./crates
COPY fuzz ./fuzz
# The release version (the git tag without its `v`), shown by the service. Declared after the
# dependency layer so a new version recompiles only our crates. Empty: Cargo.toml's version.
ARG FANFOLD_VERSION=
RUN if [ -z "${FANFOLD_VERSION}" ]; then unset FANFOLD_VERSION; else export FANFOLD_VERSION; fi \
 && cargo build --release --locked --package fanfold-server \
 && install -D target/release/fanfold /out/fanfold

# ------------------------------------------------------------------------ rust licences
# Runs beside `build` (it needs only the dependency layer, so a source change does not redo
# it), and fails the whole image build if a dependency has a licence that is not allowed.
FROM deps AS rust-licenses
COPY --from=about-tool /usr/local/cargo/bin/cargo-about /usr/local/cargo/bin/cargo-about
COPY licenses/about.toml licenses/about.hbs /licenses/
RUN mkdir /out && cargo about generate --locked --manifest-path crates/server/Cargo.toml \
        --config /licenses/about.toml /licenses/about.hbs \
        --output-file /out/THIRD-PARTY-LICENSES-RUST.txt

# ----------------------------------------------------------------------- FanFicFare
# Built apart from the runtime so no pip cache or build leftovers ship.
FROM ghcr.io/astral-sh/uv:0.13.9@sha256:d3b601f0c7b118bc1753febf7c41cc80981df678445e0254284eecf84db79a7d AS uv

FROM python-base AS venv
# Reproducible bytecode: with SOURCE_DATE_EPOCH set, Python writes hash-checked .pyc files
# instead of ones stamped with the source file's mtime.
ARG SOURCE_DATE_EPOCH
# Exactly what python/uv.lock says: every package pinned and hash-checked, no resolution
# (--locked fails if pyproject.toml and the lock disagree), and wheels only (--no-build: nothing
# is compiled here, so nothing can differ). uv itself is only in this stage, never in the image,
# and the venv has no pip (nothing installs at run time).
COPY --from=uv /uv /usr/local/bin/uv
COPY python/pyproject.toml python/uv.lock /deps/
RUN UV_PROJECT_ENVIRONMENT=/opt/venv UV_PYTHON_DOWNLOADS=never UV_LINK_MODE=copy \
    UV_COMPILE_BYTECODE=1 UV_NO_CACHE=1 \
    uv sync --project /deps --locked --no-build --no-dev --python /usr/local/bin/python \
 && /opt/venv/bin/python -I -c "import fanficfare"
# The licence notice for everything in the venv. The script exits non-zero on a licence that
# is not allowed, which fails the build (see licenses/python_licenses.py).
COPY licenses/python_licenses.py /licenses/python_licenses.py
RUN mkdir /out && /opt/venv/bin/python -I /licenses/python_licenses.py > /out/THIRD-PARTY-LICENSES-PYTHON.txt

# --------------------------------------------------------------------------- runtime
FROM python-base AS runtime

# Who the service runs as. Any other uid works too, with `--user UID:GID` (or runAsUser):
# the writable directories below are group-writable for exactly that reason, so pass the
# group this image is built with (APP_GID) and everything keeps working. Under rootless
# Docker/Podman the uid is a range inside your user namespace, so the default is fine.
ARG APP_UID=10001
ARG APP_GID=10001
# useradd stamps /etc/shadow with "last password change" = today; with this set it uses the
# commit's date instead.
ARG SOURCE_DATE_EPOCH

# No apt: the base image already ships ca-certificates, and an apt install would pull whatever
# Debian has that day (and leave logs behind), which no two builds would agree on.
RUN groupadd --system --gid "${APP_GID}" fanfold \
 && useradd --system --uid "${APP_UID}" --gid "${APP_GID}" --no-create-home \
        --home-dir /tmp/fanfold --shell /usr/sbin/nologin fanfold \
 # The only writable places. Mount a volume at /library (the books) and an empty tmpfs or
 # emptyDir at /tmp/fanfold (per-download scratch) and /run/fanfold (the rendered personal.ini,
 # best on a memory-backed mount so credentials never touch disk). They exist here so a
 # container started without mounts still works, and are 0770 so a different uid in the
 # same group can use them.
 && install -d -o "${APP_UID}" -g "${APP_GID}" -m 0770 /library /tmp/fanfold /run/fanfold \
 # Where the config file is mounted (root-owned, read-only).
 && install -d -m 0755 /etc/fanfold

COPY --from=venv /opt/venv /opt/venv
COPY --from=build /out/fanfold /usr/local/bin/fanfold
# Licences: Fanfold's own (any one of the three), and the notices for what it contains.
COPY LICENSE-0BSD LICENSE-MIT LICENSE-APACHE /usr/share/doc/fanfold/
COPY --from=rust-licenses /out/THIRD-PARTY-LICENSES-RUST.txt /usr/share/doc/fanfold/
COPY --from=venv /out/THIRD-PARTY-LICENSES-PYTHON.txt /usr/share/doc/fanfold/

LABEL org.opencontainers.image.licenses="0BSD OR MIT OR Apache-2.0"

ENV FANFOLD_CONFIG=/etc/fanfold/config.toml \
    HOME=/tmp/fanfold \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1
USER ${APP_UID}:${APP_GID}
WORKDIR /tmp/fanfold

# UI (web.listen) and ops: /healthz, /readyz, /metrics (web.ops_listen).
EXPOSE 8080 9090
# No VOLUME instruction: it would silently create anonymous volumes when a mount is
# forgotten, and the library must be mounted on purpose.

ENTRYPOINT ["/usr/local/bin/fanfold"]
CMD ["run"]
