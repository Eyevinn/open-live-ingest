#!/usr/bin/env bash
# Move every Strom pin to one tag: the strom-types tag in Cargo.toml, Cargo.lock,
# STROM_VERSION in install.sh, and the version the README names. Run from the
# repository root:
#
#   .github/scripts/bump-strom-pin.sh v0.8.1
#
# The strom-pin workflow runs it; it works by hand as well. It only edits files and
# runs `cargo update -p strom-types`; committing is left to the caller. Any other
# file still naming the old tag is listed on stdout as `leftover: <file:line>`, so
# the caller can flag it for review.

set -euo pipefail

tag="${1:-}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "usage: $0 vX.Y.Z (a stable Strom tag, got '$tag')" >&2
    exit 2
fi

current="$(sed -n 's/^strom-types = .*tag = "\([^"]*\)".*/\1/p' Cargo.toml)"
if [[ ! "$current" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
    echo "cannot read the strom-types tag from Cargo.toml (got '$current')" >&2
    exit 1
fi
# The tags as regex patterns: the only special characters they can hold are dots.
tag_re="${tag//./\\.}"
current_re="${current//./\\.}"
lock_pins_tag() { grep -q "strom\.git?tag=$tag_re#" Cargo.lock; }
if [[ "$current" == "$tag" ]]; then
    if lock_pins_tag; then
        echo "Cargo.toml and Cargo.lock already pin strom-types to $tag" >&2
        exit 0
    fi
    # An earlier run edited the files but `cargo update` did not finish.
    echo "Cargo.toml already pins strom-types to $tag but Cargo.lock does not; updating it" >&2
    cargo update -p strom-types
    lock_pins_tag || { echo "Cargo.lock does not resolve strom-types from tag $tag" >&2; exit 1; }
    exit 0
fi

sed -i -E "/^strom-types = /s/tag = \"[^\"]*\"/tag = \"$tag\"/" Cargo.toml
sed -i -E "s/^STROM_VERSION=\"[^\"]*\"/STROM_VERSION=\"$tag\"/" install.sh

readme_had_mention=false
if grep -q "Strom $current_re\b" README.md; then
    readme_had_mention=true
    sed -i -E "s/Strom $current_re\b/Strom $tag/g" README.md
fi

cargo update -p strom-types

# Check the result rather than trust the edits: each place must now name the tag.
fail=0
grep -q "^strom-types = .*tag = \"$tag_re\"" Cargo.toml \
    || { echo "Cargo.toml does not pin strom-types to $tag" >&2; fail=1; }
grep -q "^STROM_VERSION=\"$tag_re\"" install.sh \
    || { echo "install.sh does not set STROM_VERSION=\"$tag\"" >&2; fail=1; }
lock_pins_tag \
    || { echo "Cargo.lock does not resolve strom-types from tag $tag" >&2; fail=1; }
if $readme_had_mention; then
    grep -q "Strom $tag_re" README.md \
        || { echo "README.md does not name Strom $tag" >&2; fail=1; }
else
    echo "warning: README.md named no 'Strom $current'; nothing changed there" >&2
fi
[[ $fail -eq 0 ]] || exit 1

# A build script that derives the pin from Cargo.toml follows by itself. A copy of
# the version anywhere else does not, so list each line that still names the old
# version (with or without its leading v) and reads like a Strom pin.
git grep -n -I -F -e "${current#v}" -- . ':(exclude)Cargo.lock' \
    | grep -i strom | grep -i pin | sed 's/^/leftover: /' || true
