#!/bin/sh
# Installs the mib binary from a Mem in Black GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/cesarferreira/mem-in-black/main/install.sh | sh
#
# MIB_VERSION      release tag to install, such as v0.1.0 (default: latest)
# MIB_INSTALL_DIR  where to put mib (default: ~/.local/bin)
set -eu

repo="cesarferreira/mem-in-black"
version="${MIB_VERSION:-latest}"
install_dir="${MIB_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
	echo "mib install: $*" >&2
	exit 1
}

case "$(uname -s)" in
Linux) os="unknown-linux-gnu" ;;
Darwin) os="apple-darwin" ;;
*) fail "unsupported system $(uname -s); build from source with cargo instead" ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch="x86_64" ;;
aarch64 | arm64) arch="aarch64" ;;
*) fail "unsupported CPU $(uname -m); build from source with cargo instead" ;;
esac
# Releases include Apple silicon Macs only. A shell running under Rosetta
# reports x86_64 on Apple silicon.
if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ]; then
	if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
		arch="aarch64"
	else
		fail "Intel Macs have no release binary; build from source with cargo instead"
	fi
fi
asset="mem-in-black-$arch-$os.tar.gz"

if [ "$version" = "latest" ]; then
	base="https://github.com/$repo/releases/latest/download"
else
	base="https://github.com/$repo/releases/download/$version"
fi

if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	fail "sha256sum or shasum is required to verify the download"
fi
command -v curl >/dev/null 2>&1 || fail "curl is required"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo "Downloading $asset ($version)"
curl -fsSL "$base/$asset" -o "$work/$asset" || fail "could not download $base/$asset"
curl -fsSL "$base/SHA256SUMS" -o "$work/SHA256SUMS" || fail "could not download $base/SHA256SUMS"

expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$work/SHA256SUMS")"
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset"
actual="$(sha256 "$work/$asset")"
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"

tar -xzf "$work/$asset" -C "$work" mib
mkdir -p "$install_dir"
install -m 755 "$work/mib" "$install_dir/mib"
echo "Installed $("$install_dir/mib" --version) to $install_dir/mib"

case ":$PATH:" in
*":$install_dir:"*) ;;
*) echo "Add $install_dir to your PATH, for example: export PATH=\"$install_dir:\$PATH\"" ;;
esac
echo "Next, run: mib install"
