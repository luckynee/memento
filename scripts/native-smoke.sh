#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <bin-dir>" >&2
    exit 2
fi

bin_dir="$1"
temp_dir="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/memento-native-smoke.XXXXXX")"
daemon_pid=""

cleanup() {
    status=$?
    trap - EXIT
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ $status -ne 0 && -f "$temp_dir/mementod.log" ]]; then
        cat "$temp_dir/mementod.log" >&2
    fi
    rm -rf "$temp_dir"
    exit "$status"
}
trap cleanup EXIT

export HOME="$temp_dir"
export XDG_CONFIG_HOME="$temp_dir/config"
export XDG_DATA_HOME="$temp_dir/data"
export XDG_STATE_HOME="$temp_dir/state"
export XDG_CACHE_HOME="$temp_dir/cache"
export MEMENTO_STORE=sqlite
export MEMENTO_BIND=127.0.0.1:17377
export MEMENTO_SQLITE_PATH="$temp_dir/memento.db"
export FASTEMBED_CACHE_DIR="$temp_dir/models"
unset MEMENTO_TOKEN MEMENTO_LOCK_PATH HF_HOME

ls -l "$bin_dir/memento" "$bin_dir/memento-mcp" "$bin_dir/mementod"
"$bin_dir/memento" --version
if (echo >/dev/tcp/127.0.0.1/17377) 2>/dev/null; then
    echo "Port 127.0.0.1:17377 is already in use" >&2
    exit 1
fi
"$bin_dir/mementod" warmup
"$bin_dir/mementod" serve >"$temp_dir/mementod.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 90); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        echo "Native daemon exited before becoming ready" >&2
        exit 1
    fi
    if curl -fsS http://127.0.0.1:17377/health; then
        break
    fi
    sleep 1
done
curl -fsS http://127.0.0.1:17377/health
"$bin_dir/memento" --url http://127.0.0.1:17377 search "native smoke test"
