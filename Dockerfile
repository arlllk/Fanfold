# syntax=docker/dockerfile:1@sha256:572cebbc2b15e6f84c077742952674889cb63cd5e9b80abdffcbe587b6f19183
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
# HARDENING. The runtime is a Docker Hardened Image: the binary, Python and the venv, the
# libraries they link, CA certificates and tzdata. No shell, no package manager, no coreutils, so
# there is nothing to `exec` into and much less to patch (look inside with an ephemeral debug
# container: `kubectl debug`, or `docker debug` locally). The image
# runs as a non-root user, needs no capabilities, and works with a
# read-only root filesystem: the only places the service writes are /library, /tmp/fanfold and
# /run/fanfold, and each can be a volume or an empty tmpfs. It also runs under rootless
# Docker/Podman and with an arbitrary --user (see APP_UID/APP_GID, and the README).
#
#   docker run --read-only --cap-drop ALL --security-opt no-new-privileges \
#     --tmpfs /tmp/fanfold:rw,nosuid,nodev,mode=0770,uid=10001,gid=10001 \
#     --tmpfs /run/fanfold:rw,nosuid,nodev,mode=0700,uid=10001,gid=10001 \
#     -v library:/library -v ./config.toml:/etc/fanfold/config.toml:ro fanfold

# ------------------------------------------------------------------------ python base
# Docker Hardened Image (dhi.io, needs `docker login dhi.io`; CI logs in with the DHI_USERNAME and
# DHI_TOKEN secrets): Python 3.15 on Debian 13 and the shared libraries it needs, no shell, no
# package manager, no coreutils. (It does carry pip, setuptools and wheel for the system Python;
# the venv has none, and the root filesystem is read-only, so nothing installs at run time.)
# Every package in it, CPython included, is recorded in /var/lib/dpkg/status.d, so vulnerability
# scanners and the release SBOM see all of it; Docker's own SBOM is in /opt/docker/sbom.
# Python lives in /usr (/usr/bin/python3): images from before DHI moved it out of /opt would
# break the venv, whose interpreter is a link to it. For the same reason this one definition is
# shared by the venv and the runtime stage: they must be the very same image. (No `-dev` build
# image is needed: uv is copied in and every RUN is exec form.) It also has what the fanfold
# binary links: glibc (Debian's, as the build stage), libm, libgcc_s. Keep the Python minor
# version in step with ci.yml (uv sync --python), the justfile (venv) and
# crates/adapters/fanficfare/worker/pyrefly.toml.
FROM dhi.io/python:3.15.2-debian13@sha256:41eb92496c2685833535984843c609368fa6e2b98dade88056ba425850288ba8 AS python-base

# ----------------------------------------------------------------------------- chef
FROM rust:1.100.0-trixie@sha256:e84068000361bf3d4a5ba2b7647f62893528b1eb9f847d7a23b493f5ce8ea254 AS chef
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

# --------------------------------------------------------------------- cargo-auditable
# Builds the binary with its dependency list embedded (a small compressed JSON section), so
# vulnerability scanners (Trivy, Grype, Syft) find every crate inside the image, not only the
# Debian and Python packages. Its own stage, cached like cargo-about.
FROM chef AS auditable-tool
# renovate: datasource=crate depName=cargo-auditable
ARG CARGO_AUDITABLE_VERSION=0.7.7
RUN cargo install cargo-auditable --version "${CARGO_AUDITABLE_VERSION}" --locked \
 && cargo auditable --version

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
COPY --from=auditable-tool /usr/local/cargo/bin/cargo-auditable /usr/local/cargo/bin/cargo-auditable
RUN if [ -z "${FANFOLD_VERSION}" ]; then unset FANFOLD_VERSION; else export FANFOLD_VERSION; fi \
 && cargo auditable build --release --locked --package fanfold-server \
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

# The base image has no shell, so every RUN below is in exec form (a JSON list), and the
# little filesystem work there is is done by its Python.
FROM python-base AS venv
# The base runs as its nonroot user (65532); building the venv in /opt needs root.
USER 0
# Reproducible bytecode: with SOURCE_DATE_EPOCH set, Python writes hash-checked .pyc files
# instead of ones stamped with the source file's mtime.
ARG SOURCE_DATE_EPOCH
# Exactly what python/uv.lock says: every package pinned and hash-checked, no resolution
# (--locked fails if pyproject.toml and the lock disagree), and wheels only (--no-build: nothing
# is compiled here, so nothing can differ). uv itself is only in this stage, never in the image,
# and the venv has no pip (nothing installs at run time).
COPY --from=uv /uv /usr/local/bin/uv
COPY python/pyproject.toml python/uv.lock /deps/
ENV UV_PROJECT_ENVIRONMENT=/opt/venv \
    UV_PYTHON_DOWNLOADS=never \
    UV_LINK_MODE=copy \
    UV_COMPILE_BYTECODE=1 \
    UV_NO_CACHE=1
RUN ["/usr/local/bin/uv", "sync", "--project", "/deps", "--locked", "--no-build", "--no-dev", \
     "--python", "/usr/bin/python3"]
RUN ["/opt/venv/bin/python", "-I", "-c", "import fanficfare"]
# The licence notice for everything in the venv. The script exits non-zero on a licence that
# is not allowed, which fails the build (see licenses/python_licenses.py).
COPY licenses/python_licenses.py /licenses/python_licenses.py
RUN ["/opt/venv/bin/python", "-I", "-B", "/licenses/python_licenses.py", \
     "--output", "/out/THIRD-PARTY-LICENSES-PYTHON.txt"]

# --------------------------------------------------------------------------- runtime
FROM python-base AS runtime
# Root only for the RUN below (the base runs as 65532); the service runs as APP_UID (USER below).
USER 0

# Who the service runs as. Any other uid works too, with `--user UID:GID` (or runAsUser):
# the writable directories below are group-writable for exactly that reason, so pass the
# group this image is built with (APP_GID) and everything keeps working. Under rootless
# Docker/Podman the uid is a range inside your user namespace, so the default is fine. (No
# passwd entry is needed: nothing looks the user up, and HOME is set below.)
ARG APP_UID=10001
ARG APP_GID=10001

# The only writable places. Mount a volume at /library (the books) and an empty tmpfs or
# emptyDir at /tmp/fanfold (per-download scratch) and /run/fanfold (the rendered personal.ini,
# best on a memory-backed mount so credentials never touch disk). They exist here so a
# container started without mounts still works, and are 0770 so a different uid in the same
# group can use them. /etc/fanfold is where the config is mounted (root-owned, read-only).
# (ARGs are plain environment variables to an exec-form RUN.)
RUN ["/usr/bin/python3", "-I", "-B", "-c", "\
import os; u, g = int(os.environ['APP_UID']), int(os.environ['APP_GID'])\n\
for p in ('/library', '/tmp/fanfold', '/run/fanfold'):\n\
    os.makedirs(p); os.chown(p, u, g); os.chmod(p, 0o770)\n\
os.makedirs('/etc/fanfold'); os.chmod('/etc/fanfold', 0o755)\n"]

COPY --from=venv /opt/venv /opt/venv
COPY --from=build /out/fanfold /usr/local/bin/fanfold
# Licences: Fanfold's own (any one of the three), and the notices for what it contains.
COPY LICENSE-0BSD LICENSE-MIT LICENSE-APACHE /usr/share/doc/fanfold/
COPY --from=rust-licenses /out/THIRD-PARTY-LICENSES-RUST.txt /usr/share/doc/fanfold/
COPY --from=venv /out/THIRD-PARTY-LICENSES-PYTHON.txt /usr/share/doc/fanfold/
# The fonts built into the binary for the generated covers.
COPY crates/adapters/epub/fonts/OFL.txt /usr/share/doc/fanfold/LICENSE-FONTS-NOTO-OFL.txt
# The scripts built into the binary for the web UI (htmx, Alpine), and Tailwind CSS's in app.css.
COPY crates/adapters/web/assets/vendor/LICENSES.txt /usr/share/doc/fanfold/THIRD-PARTY-LICENSES-JS.txt

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
