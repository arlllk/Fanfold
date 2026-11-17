#!/usr/bin/env bash
# Run a command against a throwaway local PostgreSQL.
#   scripts/with-postgres.sh cargo test -p fanfold-adapter-postgres -- --ignored
# Needs initdb and pg_ctl on PATH. The server and its data directory are removed on exit.
set -euo pipefail

dir=$(mktemp -d)
port=${PGTEST_PORT:-54329}
cleanup() {
    pg_ctl -D "$dir/data" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$dir"
}
trap cleanup EXIT

initdb -D "$dir/data" -U postgres --auth=trust --no-sync >/dev/null
pg_ctl -D "$dir/data" -w -l "$dir/server.log" \
    -o "-p $port -k $dir -c listen_addresses=127.0.0.1 -c fsync=off" start >/dev/null

export TEST_DATABASE_URL="postgres://postgres@127.0.0.1:$port/postgres"
"$@"
