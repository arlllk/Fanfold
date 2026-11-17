# syntax=docker/dockerfile:1
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
# are written out in full with their digest on the FROM line, and the two tool versions carry a
# `# renovate:` comment naming their datasource. Keep that shape when editing this file.
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
FROM python:3.13-slim-trixie@sha256:e48f59cdba69a3cbd9ed2cf0f348d90d45a8d350d98134cf1070bc106ee9ed90 AS python-base

# ----------------------------------------------------------------------------- chef
FROM rust:1.100.0-trixie@sha256:e19840879045c951c905d0eaa9887f3439c62000e429963371b376bac8d38c31 AS chef
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
RUN cargo build --release --locked --package fanfold-server \
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
FROM python-base AS venv
# renovate: datasource=pypi depName=FanFicFare
ARG FANFICFARE_VERSION=4.63.0
# The venv has no pip of its own (it is not needed at run time); the base image's pip installs
# into it from outside.
RUN python -m venv --without-pip /opt/venv \
 && pip --python /opt/venv/bin/python install --no-cache-dir "fanficfare==${FANFICFARE_VERSION}" \
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

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid "${APP_GID}" fanfold \
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
