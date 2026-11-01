#!/usr/bin/env bash
# Build the Linux x86_64 archive: Quire-<v>-linux-x86_64.tar.gz
#
#   packaging/linux/package.sh
#
# The archive is reproducible: fixed file order, owner and timestamps
# (SOURCE_DATE_EPOCH, defaulting to the last commit time).
set -euo pipefail
cd "$(dirname "$0")/../.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
target="x86_64-unknown-linux-gnu"
name="Quire-$version-linux-x86_64"
out="dist"
stage="$out/$name"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"

cargo build --release --locked --target "$target" -p quire-desktop -p quire-cli

rm -rf "$stage" && mkdir -p "$stage"
install -m 755 "target/$target/release/quire" "target/$target/release/quire-cli" "$stage/"
install -m 644 packaging/linux/quire.desktop "$stage/"
install -m 644 packaging/icons/quire-256.png "$stage/quire.png"
install -m 755 packaging/linux/install.sh "$stage/"
install -m 644 README.md LICENSE "$stage/"

tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$out" -cf - "$name" | gzip -9n > "$out/$name.tar.gz"
rm -rf "$stage"
( cd "$out" && sha256sum "$name.tar.gz" > "$name.sha256" )
echo "Built:"; ls -la "$out/$name"*
