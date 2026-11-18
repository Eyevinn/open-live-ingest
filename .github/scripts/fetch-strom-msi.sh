#!/usr/bin/env bash
# Downloads the Strom MSI for Windows x86_64 that the Windows installer embeds
# (packaging/windows/open-live-ingest.nsi), checks it against the SHA-256 digest GitHub
# records for the asset, as install.ps1 does, and prints its path. No digest, or a
# different one, and it fails. From the repository root:
#
#   .github/scripts/fetch-strom-msi.sh <dir>     # prints <dir>/strom-<x.y.z>-windows-x86_64.msi
#
# The Strom version is the strom-types tag in Cargo.toml, the same pin build.rs,
# install.ps1 and build-macos-app.sh use, so the release workflow never names one.
# Needs gh, with GH_TOKEN set (the release is public; the token lifts the rate limit).
set -euo pipefail

die() { echo "fetch-strom-msi.sh: $*" >&2; exit 1; }
[[ $# -eq 1 && -n "$1" ]] || die "usage: fetch-strom-msi.sh <dir>"
dir="$1"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

strom_tag="$(sed -n 's/^strom-types = .*tag = "\([^"]*\)".*/\1/p' "$root/Cargo.toml")"
[[ "$strom_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || die "cannot read the strom-types tag from Cargo.toml (got '$strom_tag')"
repo=Eyevinn/strom
asset="strom-${strom_tag#v}-windows-x86_64.msi"

digest="$(gh api "repos/$repo/releases/tags/$strom_tag" \
    --jq ".assets[] | select(.name == \"$asset\") | .digest")"
[[ "$digest" == sha256:* ]] || die "GitHub records no SHA-256 digest for $asset of Strom $strom_tag"

mkdir -p "$dir"
gh release download "$strom_tag" --repo "$repo" --pattern "$asset" --dir "$dir" --clobber
have="sha256:$(sha256sum "$dir/$asset" | cut -d ' ' -f 1)"
[[ "$have" == "$digest" ]] \
    || die "$asset: $have does not match the digest GitHub records ($digest)"
echo "Strom $strom_tag: $asset matches $digest" >&2
echo "$dir/$asset"
