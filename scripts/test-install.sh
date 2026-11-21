#!/bin/sh
# Tests install.sh against a fake release in a temporary directory: a good install, a tampered archive and a
# missing checksum. Run from the repository root: sh scripts/test-install.sh
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
    *) echo "unsupported test platform" >&2; exit 1 ;;
esac

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1; else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}

# A release whose "binary" is a script that prints a version.
release="$work/release"
mkdir -p "$release" "$work/stage/zenodotus-$target"
printf '#!/bin/sh\necho "zenodotus 9.9.9"\n' >"$work/stage/zenodotus-$target/zenodotus"
chmod 755 "$work/stage/zenodotus-$target/zenodotus"
tar -czf "$release/zenodotus-$target.tar.gz" -C "$work/stage" "zenodotus-$target"
echo "$(sha256 "$release/zenodotus-$target.tar.gz")  zenodotus-$target.tar.gz" >"$release/zenodotus-$target.tar.gz.sha256"

failures=0
check() {
    if eval "$2"; then echo "ok: $1"; else echo "FAILED: $1" >&2; failures=$((failures + 1)); fi
}

# 1. A good install lands in the install directory and runs.
out="$(ZENODOTUS_BASE_URL="file://$release" ZENODOTUS_INSTALL_DIR="$work/bin" sh "$root/install.sh" 2>&1)"
check "installs the binary" '[ -x "$work/bin/zenodotus" ]'
check "the installed binary runs" '[ "$("$work/bin/zenodotus")" = "zenodotus 9.9.9" ]'
check "reports the version and checksum" 'echo "$out" | grep -q "Installed zenodotus 9.9.9 to $work/bin/zenodotus (sha256 "'
check "leaves no temporary file" '[ ! -e "$work/bin/zenodotus.tmp" ]'

# 2. A tampered archive is refused and nothing is installed.
# The tampered archive is a valid one with other contents, so only the checksum can catch it.
cp -R "$release" "$work/tampered"
printf '#!/bin/sh\necho "tampered"\n' >"$work/stage/zenodotus-$target/zenodotus"
tar -czf "$work/tampered/zenodotus-$target.tar.gz" -C "$work/stage" "zenodotus-$target"
if out="$(ZENODOTUS_BASE_URL="file://$work/tampered" ZENODOTUS_INSTALL_DIR="$work/bin2" sh "$root/install.sh" 2>&1)"; then
    status=0
else
    status=$?
fi
check "a tampered archive fails" '[ "$status" -ne 0 ]'
check "says the checksum does not match" 'echo "$out" | grep -q "checksum mismatch"'
check "installs nothing after a mismatch" '[ ! -e "$work/bin2" ]'

# 3. A missing checksum file is refused.
cp -R "$release" "$work/nosum"
rm "$work/nosum/zenodotus-$target.tar.gz.sha256"
if out="$(ZENODOTUS_BASE_URL="file://$work/nosum" ZENODOTUS_INSTALL_DIR="$work/bin3" sh "$root/install.sh" 2>&1)"; then
    status=0
else
    status=$?
fi
check "a missing checksum fails" '[ "$status" -ne 0 ]'
check "installs nothing without a checksum" '[ ! -e "$work/bin3" ]'

if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "install.sh: all checks passed"
