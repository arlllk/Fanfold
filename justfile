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

# One-time: a venv with the pinned FanFicFare, for the integration tests.
venv:
    python3 -m venv .venv
    .venv/bin/pip install --quiet 'fanficfare==4.63.0'

# The shared port contracts, run against a real PostgreSQL.
test-postgres:
    scripts/with-postgres.sh cargo test -p fanfold-adapter-postgres -- --ignored

# The real Python worker, against FanFicFare's offline test site (no network).
test-worker:
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

ci: fmt-check lint test integration

fmt-check:
    cargo fmt --all -- --check

# The licence notices the container image ships (needs: cargo install cargo-about --features cli).
# The Python list is for the local .venv, which also contains pip; the image's venv has no pip.
licenses:
    mkdir -p target/licenses
    cargo about generate --locked --manifest-path crates/server/Cargo.toml --config licenses/about.toml licenses/about.hbs --output-file target/licenses/THIRD-PARTY-LICENSES-RUST.txt
    {{ffd_python}} -I licenses/python_licenses.py > target/licenses/THIRD-PARTY-LICENSES-PYTHON.txt
