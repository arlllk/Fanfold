# Task runner. `just` lists recipes; `just ci` is the full local gate.
set shell := ["bash", "-euo", "pipefail", "-c"]

# Python with FanFicFare installed, used by the integration tests.
ffd_python := env_var_or_default("FFD_PYTHON", justfile_directory() / ".venv/bin/python")

default:
    @just --list

# --- the quick loop -------------------------------------------------------------

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# --- tests that need real services (throwaway local PostgreSQL, real Python) ------

# A venv with exactly the FanFicFare the image ships (python/uv.lock), for the integration
# tests. Needs uv. Re-run after the lock changes.
venv:
    UV_PROJECT_ENVIRONMENT={{justfile_directory()}}/.venv uv sync --project python --locked --no-build --python 3.15

# The shared port contracts, run against a real PostgreSQL.
test-postgres:
    scripts/with-postgres.sh cargo test -p fanfold-adapter-postgres -- --ignored

# The Python worker, type-checked against the pinned FanFicFare it hooks into (needs `just venv`).
lint-python:
    {{parent_directory(ffd_python)}}/pyrefly check --config crates/adapters/fanficfare/worker/pyrefly.toml --python-interpreter-path {{ffd_python}}
    {{parent_directory(ffd_python)}}/pyrefly check --config scripts/pyrefly.toml --python-interpreter-path {{ffd_python}}

# The real Python worker: its own unit tests, then through FanFicFare against its offline
# test site and a fake fanfold-browser (no network).
# fanfold-browser's queue, pacing and cool-down, with a fake browser (no Camoufox needed).
test-browser:
    python3 -I -B -m unittest discover -s browser

# The fanfold-browser image (Camoufox inside; large).
browser-image:
    docker build -f browser/Dockerfile -t fanfold-browser .

test-worker:
    {{ffd_python}} -I -B -m unittest discover -s crates/adapters/fanficfare/worker
    {{ffd_python}} -I -B -m unittest discover -s scripts
    FFD_PYTHON={{ffd_python}} cargo test -p fanfold-adapter-fanficfare -- --ignored

# Config -> secrets -> PostgreSQL + filesystem + FanFicFare -> worker.
e2e:
    FFD_PYTHON={{ffd_python}} scripts/with-postgres.sh cargo test -p fanfold-server --test e2e -- --ignored

integration: test-postgres test-worker e2e

# --- the UI -----------------------------------------------------------------------

# The real UI over realistic in-memory data: http://127.0.0.1:8099 (add `-- --degraded` for the outage state).
demo *args:
    cargo run -p fanfold-adapter-web --example demo {{args}}

# The mail trigger against a REAL mailbox, read-only (see README, "Mail"). Needs
# JMAP_UPSTREAM, JMAP_PUBLIC_ORIGIN, JMAP_USER and JMAP_PASSWORD in the environment.
live-mail:
    cargo test -p fanfold-adapter-jmap --test live -- --ignored --nocapture --test-threads=1

# --- deeper verification ---------------------------------------------------------

# Mutation testing of the logic crates. Slow; surviving mutants are test gaps.
mutants *args:
    cargo mutants -p fanfold-domain -p fanfold-app -p fanfold-adapter-web {{args}}

# Fuzz one target (needs nightly): just fuzz work_url 60
fuzz target seconds="60":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}} -max_len=4096

# Every fuzz target for a short time.
fuzz-all seconds="30":
    for t in work_url mail_links book_file_name worker_output config ini_render discord_sanitize form_ids csrf event_payload html_escape jmap_email sender_filter; do \
        just fuzz $t {{seconds}}; \
    done

# --- everything -------------------------------------------------------------------

ci: fmt-check lint lint-python test test-browser integration

fmt-check:
    cargo fmt --all -- --check

# The licence notices the container image ships (needs: cargo install cargo-about --features cli).
# The Python list is for the local .venv, built from the same lock as the image's venv.
licenses:
    mkdir -p target/licenses
    cargo about generate --locked --manifest-path crates/server/Cargo.toml --config licenses/about.toml licenses/about.hbs --output-file target/licenses/THIRD-PARTY-LICENSES-RUST.txt
    {{ffd_python}} -I licenses/python_licenses.py > target/licenses/THIRD-PARTY-LICENSES-PYTHON.txt
