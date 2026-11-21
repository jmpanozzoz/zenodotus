#!/bin/sh
# Installs zenodotus from a GitHub release into ~/.local/bin, without root, after checking the archive's SHA-256.
#
#   curl -fsSL https://raw.githubusercontent.com/jmpanozzoz/zenodotus/main/install.sh | sh
#
# Settings, all optional, from the environment:
#   ZENODOTUS_VERSION      a release tag such as v0.1.0; the latest release by default
#   ZENODOTUS_INSTALL_DIR  where the binary goes; ~/.local/bin by default
#   ZENODOTUS_REPO         the GitHub repository; jmpanozzoz/zenodotus by default
#   ZENODOTUS_BASE_URL     a URL or file:// directory holding the release assets, instead of GitHub
set -eu

repo="${ZENODOTUS_REPO:-jmpanozzoz/zenodotus}"
version="${ZENODOTUS_VERSION:-latest}"
install_dir="${ZENODOTUS_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"

fail() {
    echo "zenodotus install: $*" >&2
    exit 1
}

target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os/$arch" in
        Linux/x86_64 | Linux/amd64) echo x86_64-unknown-linux-musl ;;
        Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-musl ;;
        Darwin/x86_64) echo x86_64-apple-darwin ;;
        Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
        *) fail "no release binary for $os/$arch; build from source with cargo install --git https://github.com/$repo zenodotus" ;;
    esac
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https,file' --tlsv1.2 -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "curl or wget is needed"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "sha256sum or shasum is needed to check the download"
    fi
}

target="$(target)"
asset="zenodotus-$target.tar.gz"
if [ -n "${ZENODOTUS_BASE_URL:-}" ]; then
    base="$ZENODOTUS_BASE_URL"
elif [ "$version" = latest ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

download "$base/$asset" "$work/$asset" || fail "cannot download $base/$asset"
download "$base/$asset.sha256" "$work/$asset.sha256" || fail "cannot download $base/$asset.sha256"
expected="$(cut -d ' ' -f 1 "$work/$asset.sha256")"
actual="$(sha256 "$work/$asset")"
[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
    fail "checksum mismatch for $asset: expected '$expected', got '$actual'; nothing was installed"

tar -xzf "$work/$asset" -C "$work"
[ -f "$work/zenodotus-$target/zenodotus" ] || fail "$asset does not hold zenodotus-$target/zenodotus"
mkdir -p "$install_dir"
cp "$work/zenodotus-$target/zenodotus" "$install_dir/zenodotus.tmp"
chmod 755 "$install_dir/zenodotus.tmp"
mv -f "$install_dir/zenodotus.tmp" "$install_dir/zenodotus"

echo "Installed $("$install_dir/zenodotus" --version) to $install_dir/zenodotus (sha256 $actual)."
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "$install_dir is not on PATH; add it, for example: export PATH=\"$install_dir:\$PATH\"" ;;
esac
echo "Next: zenodotus setup claude-code   (or cursor, codex, vscode, generic)"
