#!/bin/sh
set -eu

version=${MEMENTO_VERSION:-latest}
install_dir=${MEMENTO_INSTALL_DIR:-"$HOME/.local/bin"}
os=$(uname -s)
arch=$(uname -m)

case "$os:$arch" in
Linux:x86_64 | Linux:amd64) target=x86_64-unknown-linux-musl ;;
Linux:aarch64 | Linux:arm64) target=aarch64-unknown-linux-musl ;;
Darwin:x86_64 | Darwin:amd64) target=x86_64-apple-darwin ;;
Darwin:arm64 | Darwin:aarch64) target=aarch64-apple-darwin ;;
*)
	printf 'Unsupported platform: %s/%s\n' "$os" "$arch" >&2
	exit 1
	;;
esac

mkdir -p "$install_dir"
install_dir=$(CDPATH= cd -- "$install_dir" && pwd -P)
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/memento-install.XXXXXX")
trap 'rm -rf "$temp_dir"' 0
trap 'exit 1' HUP INT TERM

asset="memento-$target.tar.gz"
if [ -n "${MEMENTO_DOWNLOAD_BASE:-}" ]; then
	base=$MEMENTO_DOWNLOAD_BASE
elif [ "$version" = latest ]; then
	base=https://github.com/luckynee/memento/releases/latest/download
else
	version=${version#v}
	base="https://github.com/luckynee/memento/releases/download/v$version"
fi

download() {
	source_url=$1
	destination=$2
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$source_url" -o "$destination"
	elif command -v wget >/dev/null 2>&1; then
		wget -q "$source_url" -O "$destination"
	else
		printf 'Neither curl nor wget is available\n' >&2
		exit 1
	fi
}

download "$base/$asset" "$temp_dir/$asset"
download "$base/$asset.sha256" "$temp_dir/$asset.sha256"
(
	cd "$temp_dir"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "$asset.sha256"
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 -c "$asset.sha256"
	else
		printf 'Neither sha256sum nor shasum is available\n' >&2
		exit 1
	fi
)
tar -xzf "$temp_dir/$asset" -C "$temp_dir"

for binary in memento memento-mcp; do
	if [ ! -f "$temp_dir/$binary" ]; then
		printf 'Release archive is missing %s\n' "$binary" >&2
		exit 1
	fi
	cp "$temp_dir/$binary" "$install_dir/.$binary.tmp.$$"
	chmod 755 "$install_dir/.$binary.tmp.$$"
	mv "$install_dir/.$binary.tmp.$$" "$install_dir/$binary"
done

installed_version=$("$install_dir/memento" --version | awk '{print $NF}')
case "$installed_version" in
'' | *[!0-9.]*)
	printf 'Could not determine installed memento version\n' >&2
	exit 1
	;;
esac

"$install_dir/memento" write-receipt --method script --bin-dir "$install_dir"

case ":${PATH:-}:" in
*":$install_dir:"*) ;;
*)
	printf 'Add this line to your shell profile:\nexport PATH="%s:$PATH"\n' "$install_dir"
	;;
esac

if ! command -v docker >/dev/null 2>&1; then
	printf 'Docker is required to run the memento daemon.\n'
fi

printf 'Installed memento %s and memento-mcp to %s\n' "$installed_version" "$install_dir"
if [ "${MEMENTO_NO_SETUP:-0}" != 1 ]; then
	if [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
		"$install_dir/memento" setup </dev/tty
	else
		printf 'Next steps: run memento setup, then memento daemon up.\n'
	fi
fi
