# syntax=docker/dockerfile:1@sha256:572cebbc2b15e6f84c077742952674889cb63cd5e9b80abdffcbe587b6f19183
#
# fanfold-browser: one Camoufox browser that makes every request Fanfold sends to AO3 (see
# fanfold_browser.py). Built from the repository root:
#
#   docker build -f browser/Dockerfile -t fanfold-browser .
#   docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
#     --tmpfs /tmp:rw,nosuid,nodev,uid=10001,gid=10001 -v browser-profile:/data \
#     -e BROWSER_PROXY=http://proxy:8080 -p 127.0.0.1:8191:8191 fanfold-browser
#
# Unlike the main image this one has a shell and a few Debian packages: Firefox needs GTK, X11
# and ALSA libraries that no distroless Python image carries. Everything else holds: every
# version is pinned (base image by digest, Python packages by hash in browser/uv.lock, the
# Camoufox browser by its package version), Renovate keeps them current, and it runs as a
# non-root user with no capabilities on a read-only root filesystem. It writes only to /data
# (the browser profile: cookies, AO3's login; keep it on a volume so a restart keeps them) and
# /tmp. The apt packages are not pinned, so the image is not reproducible byte for byte.

FROM ghcr.io/astral-sh/uv:0.13.10@sha256:a3ea1357342c1a4bf9a0c53a3deb1b55379aefa037338d8113d5771efa01774f AS uv

# Python 3.13 (see browser/pyproject.toml for why not 3.15).
FROM python:3.13-slim-trixie@sha256:e48f59cdba69a3cbd9ed2cf0f348d90d45a8d350d98134cf1070bc106ee9ed90

# What Firefox links and nothing more (no recommends).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libgtk-3-0t64 libx11-xcb1 libasound2t64 libdbus-glib-1-2 libxtst6 fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/*

ARG APP_UID=10001
ARG APP_GID=10001

# The venv, exactly as browser/uv.lock says (hash-checked, no resolution, wheels only).
COPY --from=uv /uv /usr/local/bin/uv
COPY browser/pyproject.toml browser/uv.lock /deps/
ENV UV_PROJECT_ENVIRONMENT=/opt/venv \
    UV_PYTHON_DOWNLOADS=never \
    UV_LINK_MODE=copy \
    UV_COMPILE_BYTECODE=1 \
    UV_NO_CACHE=1 \
    # Camoufox keeps its browser and GeoIP database under the cache directory: in the image,
    # read-only, so nothing is downloaded at run time.
    XDG_CACHE_HOME=/opt/cache
# The browser presents as Linux (fanfold_browser.py: BROWSER_OS), so only the font groups Linux
# reads (those named with an L, see fonts/groups.json) are kept: 0.9 of the bundle's 2.1 GB.
# Camoufox writes a generated font config under its cache directory at every launch; that one
# directory points into /tmp (the service creates it), the rest stays read-only.
RUN uv sync --project /deps --locked --no-build --no-dev --python /usr/local/bin/python3 \
 && /opt/venv/bin/python -m camoufox fetch \
 && /opt/venv/bin/python -I -c "import pathlib, shutil; \
[shutil.rmtree(d) for d in pathlib.Path('/opt/cache/camoufox/browsers').glob('*/*/fonts/*') \
 if d.is_dir() and 'L' not in d.name]" \
 && ln -s /tmp/camoufox-fontconfig /opt/cache/camoufox/fontconfig \
 && rm /usr/local/bin/uv \
 && chmod -R a+rX /opt/cache \
 && install -d -o "${APP_UID}" -g "${APP_GID}" -m 0770 /data

COPY browser/fanfold_browser.py /app/fanfold_browser.py
COPY LICENSE-0BSD LICENSE-MIT LICENSE-APACHE /usr/share/doc/fanfold-browser/

LABEL org.opencontainers.image.licenses="0BSD OR MIT OR Apache-2.0" \
      org.opencontainers.image.description="One Camoufox browser that makes every request Fanfold sends to AO3"

ENV HOME=/tmp \
    PROFILE_DIR=/data/profile \
    RUNTIME_DIRS=/tmp/camoufox-fontconfig \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1
USER ${APP_UID}:${APP_GID}
EXPOSE 8191
ENTRYPOINT ["/opt/venv/bin/python", "-I", "/app/fanfold_browser.py"]
