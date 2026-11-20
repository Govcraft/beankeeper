#!/bin/sh
# Offline release preflight. Build bk separately, then run this script.
set -eu

export LC_ALL=C
ROOT="$(CDPATH='' \cd -- "$(dirname -- "$0")/.." && pwd)"

fail() { printf 'release preflight: %s\n' "$1" >&2; exit 1; }
usage() {
    printf 'Usage: %s [--metadata-only] [--tag beankeeper-cli-vX.Y.Z] [PATH_TO_BK]\n' "$0"
}

METADATA_ONLY=false
TAG=''
BK_BINARY=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --metadata-only) METADATA_ONLY=true ;;
        --tag)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail '--tag requires a tag name'
            TAG="$2"
            shift
            ;;
        --help|-h) usage; exit 0 ;;
        -*) usage >&2; fail "unknown option: $1" ;;
        *)
            [ -z "$BK_BINARY" ] || fail 'provide only one binary path'
            BK_BINARY="$1"
            ;;
    esac
    shift
done

# Read only the literal version in [package], never a dependency's version.
VERSION="$(awk '
    /^[[:space:]]*\[/ {
        in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/)
    }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
        split($0, fields, "\"")
        print fields[2]
    }
' "$ROOT/beankeeper-cli/Cargo.toml")"
printf '%s\n' "$VERSION" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
    || fail 'beankeeper-cli/Cargo.toml must declare a stable X.Y.Z package version'

[ -f "$ROOT/CHANGELOG.md" ] || fail 'CHANGELOG.md is missing'
awk -v version="$VERSION" '
    $1 == "##" && $2 == "[" version "]" { found = 1 }
    END { exit !found }
' "$ROOT/CHANGELOG.md" || fail "CHANGELOG.md needs a section headed ## [$VERSION]"

if [ -n "$TAG" ]; then
    [ "$TAG" = "beankeeper-cli-v$VERSION" ] \
        || fail "tag '$TAG' must equal beankeeper-cli-v$VERSION"
    awk -v version="$VERSION" '
        $1 == "##" && $2 == "[" version "]" && $3 == "-" && NF == 4 &&
            $4 ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/ { found = 1 }
        END { exit !found }
    ' "$ROOT/CHANGELOG.md" \
        || fail "CHANGELOG.md needs a dated section headed ## [$VERSION] - YYYY-MM-DD for a tagged release"
elif [ "$METADATA_ONLY" = true ]; then
    awk '
        $1 == "##" && $2 == "[Unreleased]" && NF == 2 { found = 1 }
        END { exit !found }
    ' "$ROOT/CHANGELOG.md" || fail 'CHANGELOG.md needs a section headed ## [Unreleased]'
fi

if [ "$METADATA_ONLY" = true ]; then
    [ -z "$BK_BINARY" ] || fail '--metadata-only cannot be combined with a binary path'
    printf 'Release metadata OK: bk %s\n' "$VERSION"
    exit 0
fi

if [ -z "$BK_BINARY" ]; then
    TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
    case "$TARGET_DIR" in
        /*) ;;
        *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
    esac
    BK_BINARY="$TARGET_DIR/debug/bk"
fi
[ -x "$BK_BINARY" ] || fail "build bk first or pass its executable path: $BK_BINARY"
ACTUAL="$("$BK_BINARY" --version)" || fail "could not execute $BK_BINARY --version"
[ "$ACTUAL" = "bk $VERSION" ] \
    || fail "binary reports '$ACTUAL', expected 'bk $VERSION'; rebuild bk"

printf 'Release preflight OK: bk %s\n' "$VERSION"
