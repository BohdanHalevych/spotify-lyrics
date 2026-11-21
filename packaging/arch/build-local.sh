#!/usr/bin/env bash
set -euo pipefail

recipe_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "$recipe_dir/../.." && pwd)
output_dir="$recipe_dir/dist"
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT

version=$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' "$project_dir/Cargo.toml")
recipe_version=$(sed -n 's/^pkgver=//p' "$recipe_dir/PKGBUILD")
if [[ -z "$version" || "$version" != "$recipe_version" ]]; then
    printf 'Cargo.toml and PKGBUILD must have the same release version.\n' >&2
    exit 1
fi
archive="$stage/lyricsync-$version.tar.zst"

# Snapshot only the source and distribution files. The local working tree may
# contain changes, so verify the exact archive used in this invocation.
tar -C "$project_dir" --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
    --transform="s,^,lyricsync-$version/," \
    -cf - Cargo.toml Cargo.lock LICENSE README.md src tests examples |
    zstd --quiet -T1 -o "$archive"
read -r checksum _ < <(sha256sum -- "$archive")
sed -E "s/^sha256sums=\('[[:xdigit:]]{64}'\)$/sha256sums=('$checksum')/" \
    "$recipe_dir/PKGBUILD" > "$stage/PKGBUILD"

mkdir -p -- "$output_dir"
cd -- "$stage"
PKGDEST="$output_dir" SRCDEST="$stage" BUILDDIR="$stage" makepkg --force
