#!/bin/sh
set -eu

root=$(CDPATH='' cd "$(dirname "$0")/.." && pwd)
cd "$root"

mode=${1-}
if [ "$#" -gt 1 ] || { [ -n "$mode" ] && [ "$mode" != "--check" ]; }; then
    printf 'usage: %s [--check]\n' "$0" >&2
    exit 2
fi

if [ "$mode" = "--check" ]; then
    version=$(awk '
        /^\[workspace\.package\]$/ { in_package = 1; next }
        /^\[/ { in_package = 0 }
        in_package && /^version[[:space:]]*=/ {
            sub(/^[^"]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ' Cargo.toml)
    if [ -z "$version" ]; then
        printf '%s\n' 'cannot find workspace package version' >&2
        exit 1
    fi
fi

for adapter in claude-code codex agy; do
    destination="adapters/$adapter/skills"
    if [ "$mode" = "--check" ]; then
        if ! diff -r skills "$destination" >/dev/null; then
            printf 'skills differ: %s\n' "$destination" >&2
            exit 1
        fi
        manifest="adapters/$adapter/plugin.json"
        if [ "$adapter" = "claude-code" ]; then
            manifest="adapters/$adapter/.claude-plugin/plugin.json"
        fi
        if ! python3 -c 'import json, sys; sys.exit(json.load(open(sys.argv[1], encoding="utf-8")).get("version") != sys.argv[2])' "$manifest" "$version"; then
            printf 'manifest version differs from %s: %s\n' "$version" "$manifest" >&2
            exit 1
        fi
    else
        rm -rf "$destination"
        mkdir -p "$destination"
        cp -R skills/. "$destination/"
    fi
done
