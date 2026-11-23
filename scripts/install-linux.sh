#!/usr/bin/env bash
# Installs the latest Querent release for Linux into ~/.local/bin, adds a
# desktop entry for app launchers, and removes a copy installed with
# `cargo install`, so `cargo run` stays the only development copy.
#
# The download must match its .sha256 and its minisign signature, made with
# the key below (the same key the app checks its own updates against).
set -euo pipefail

repo="MoXcz/querent"
public_key="RWRfKVM3V2IJxWQ1OD8aCHLILKpHwB4YeVzHwVhW+7AOD1EKQyDBBg3A"
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
apps_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

fail() {
    echo "install-linux.sh: $*" >&2
    exit 1
}

for tool in curl tar sha256sum; do
    command -v "$tool" > /dev/null || fail "needs $tool"
done
if command -v minisign > /dev/null; then
    verify() { minisign -Vm "$1" -P "$public_key" -x "$1.minisig"; }
elif command -v rsign > /dev/null; then
    verify() { rsign verify -P "$public_key" -x "$1.minisig" "$1"; }
else
    fail "needs minisign (pacman -S minisign) or rsign (cargo install rsign2) to check the signature"
fi
[ "$(uname -m)" = x86_64 ] || fail "releases are built for x86_64 only"

latest=$(curl -fsSLI --proto '=https' -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
tag=${latest##*/}
[[ "$tag" =~ ^v[0-9]+(\.[0-9]+)*$ ]] || fail "could not find the latest release (got $latest)"

archive="querent-$tag-x86_64-linux.tar.gz"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

echo "Downloading Querent $tag"
for file in "$archive" "$archive.sha256" "$archive.minisig"; do
    curl -fsSL --proto '=https' -o "$file" "https://github.com/$repo/releases/download/$tag/$file"
done

sha256sum --check --quiet "$archive.sha256" || fail "checksum mismatch"
output=$(verify "$archive" 2>&1) || fail "signature check failed: $output"
# The trusted comment names the archive, so a signed file from another release is refused.
grep -qxF "Trusted comment: $archive" <<< "$output" || fail "the signature is for another file"

tar -xzf "$archive"
binary="querent-$tag-x86_64-linux/querent"
[ -f "$binary" ] || fail "the archive has no querent binary"

mkdir -p "$bin_dir" "$apps_dir"
# Replace by rename, so a running copy keeps working until it restarts.
staged=$(mktemp "$bin_dir/.querent.XXXXXX")
install -m 755 "$binary" "$staged"
mv -f "$staged" "$bin_dir/querent"

cat > "$apps_dir/querent.desktop" << EOF
[Desktop Entry]
Type=Application
Name=Querent
GenericName=SQL Editor
Comment=SQL editor for SQL Server, Fabric, PostgreSQL, SQLite and Snowflake
Exec="$bin_dir/querent"
Icon=libreoffice-oasis-database
Terminal=false
Categories=Development;Database;
Keywords=sql;database;query;postgres;sqlite;
EOF
if command -v update-desktop-database > /dev/null; then
    update-desktop-database "$apps_dir" || true
fi

cargo_copy="${CARGO_HOME:-$HOME/.cargo}/bin/querent"
if [ "$cargo_copy" != "$bin_dir/querent" ] && { [ -e "$cargo_copy" ] || [ -L "$cargo_copy" ]; }; then
    if command -v cargo > /dev/null && cargo uninstall querent > /dev/null 2>&1; then
        echo "Removed the cargo-installed copy"
    else
        echo "Left $cargo_copy in place: cargo did not install it; remove it by hand if it is an old copy."
    fi
fi

echo "Installed Querent $tag to $bin_dir/querent"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Note: $bin_dir is not on PATH; the desktop entry still works." ;;
esac
