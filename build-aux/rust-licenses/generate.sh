#!/usr/bin/env bash
# Generates THIRD-PARTY-LICENSES.txt, the licences of the Rust crates the
# linux-image-writer GUI is built from, checks it (check.py), and keeps the
# repository's copy, data/THIRD-PARTY-LICENSES.txt, up to date. See README.md.
#
#   generate.sh [--check] [--output FILE] [--manifest FILE] [--binary FILE]
#
#   (no --check)     generate, check, and write data/THIRD-PARTY-LICENSES.txt
#                    (the document AppImage and Flatpak ship; commit it)
#   --check          generate and check, then fail unless
#                    data/THIRD-PARTY-LICENSES.txt is byte for byte what was
#                    generated: it is stale whenever Cargo.lock, about.toml,
#                    the template, a crate's licence files or cargo-about
#                    changed and it was not regenerated. Writes nothing in
#                    data/.
#   --output FILE    also keep the generated document as FILE
#   --manifest FILE  write rust-license-manifest.txt (the per-crate record)
#   --binary FILE    the built GUI: its embedded source paths are checked
#                    against the crates covered
#
# Nothing is written unless every check passes (and, with --check, the
# repository's copy is current), so a failed run leaves no output.
#
# Needs cargo, python3 and the cargo-about that cargo-about.lock pins (the
# AppImage build container has all three: build-aux/appimage/Containerfile).
# Fetches the crates Cargo.lock lists if they are missing (verified by
# Cargo against Cargo.lock); everything after that runs offline, so the
# output depends only on Cargo.lock, about.toml, the template and the tool.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
ROOT_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly SCRIPT_DIR ROOT_DIR
readonly LOCK=$SCRIPT_DIR/cargo-about.lock
readonly CONFIG=$SCRIPT_DIR/about.toml
readonly TEMPLATE=$SCRIPT_DIR/THIRD-PARTY-LICENSES.txt.hbs
# The repository's copy: the source of truth for every packaging.
readonly REPO_DOCUMENT_REL=data/THIRD-PARTY-LICENSES.txt
readonly REPO_DOCUMENT=$ROOT_DIR/$REPO_DOCUMENT_REL

fail() { printf 'generate.sh: error: %s\n' "$*" >&2; exit 1; }
usage() {
    printf 'usage: %s [--check] [--output FILE] [--manifest FILE] [--binary FILE]\n' "${0##*/}" >&2
    exit 2
}

CHECK=0 OUTPUT='' MANIFEST='' BINARY=''
while [[ $# -gt 0 ]]; do
    case $1 in
        --check) CHECK=1; shift ;;
        --output) [[ $# -ge 2 ]] || usage; OUTPUT=$2; shift 2 ;;
        --manifest) [[ $# -ge 2 ]] || usage; MANIFEST=$2; shift 2 ;;
        --binary) [[ $# -ge 2 ]] || usage; BINARY=$2; shift 2 ;;
        *) usage ;;
    esac
done
readonly CHECK OUTPUT MANIFEST BINARY

# The value of KEY in cargo-about.lock (read as text, never evaluated).
lock_value() {
    local value
    value=$(sed -n "s/^$1=//p" -- "$LOCK")
    [[ -n $value && $value != *$'\n'* ]] || fail "cargo-about.lock: expected exactly one $1"
    printf '%s' "$value"
}

WORK=
cleanup() {
    local status=$?
    [[ -z $WORK ]] || rm -rf -- "$WORK"
    [[ $status -eq 0 ]] || printf 'generate.sh: FAILED (exit %d); nothing was written.\n' "$status" >&2
}
trap cleanup EXIT

# The pinned cargo-about, and nothing else: its executable's SHA-256 must be
# the one cargo-about.lock records.
cargo_about=$(command -v cargo-about) || fail "cargo-about not found (install the version cargo-about.lock pins)"
version=$(lock_value CARGO_ABOUT_VERSION)
binary_sha256=$(lock_value CARGO_ABOUT_BINARY_SHA256)
printf '%s  %s\n' "$binary_sha256" "$cargo_about" | sha256sum --check --status \
    || fail "$cargo_about is not the cargo-about $version executable cargo-about.lock pins (SHA-256 differs)"
[[ $("$cargo_about" --version) == "cargo-about $version" ]] \
    || fail "$cargo_about reports '$("$cargo_about" --version)', expected cargo-about $version"
command -v python3 >/dev/null || fail "python3 not found"
[[ -z $BINARY || -f $BINARY ]] || fail "no such binary: $BINARY"
printf '==> cargo-about %s (%s)\n' "$version" "$binary_sha256"

WORK=$(mktemp -d)
readonly GENERATED=$WORK/THIRD-PARTY-LICENSES.txt
cd -- "$ROOT_DIR"

# Every crate Cargo.lock lists, for every platform (cargo-about reads the
# whole package graph through cargo metadata before filtering it).
printf '==> Fetching the crates Cargo.lock lists (if missing)\n'
cargo fetch --locked --quiet

# The same run twice: as JSON, for the checks, and through the template.
# --frozen: Cargo.lock as it is, and no network (so no licence file is ever
# fetched from a crate's git repository). --fail: a licence expression that
# cannot be satisfied or resolved is an error, not a warning.
about() {
    "$cargo_about" generate --frozen --fail --features gui --config "$CONFIG" "$@"
}
printf '==> Generating with cargo-about\n'
about --format json --output-file "$WORK/about.json" 2>"$WORK/json.stderr" \
    || { cat -- "$WORK/json.stderr" >&2; fail "cargo-about failed"; }
about --output-file "$GENERATED" "$TEMPLATE" 2>"$WORK/document.stderr" \
    || { cat -- "$WORK/document.stderr" >&2; fail "cargo-about failed"; }

printf '==> Checking the document against the dependency graph\n'
python3 "$SCRIPT_DIR/check.py" --root "$ROOT_DIR" --json "$WORK/about.json" \
    --document "$GENERATED" --manifest "$WORK/rust-license-manifest.txt" \
    --stderr "$WORK/json.stderr" "$WORK/document.stderr" \
    ${BINARY:+--binary "$BINARY"}
generated_sha256=$(sha256sum -- "$GENERATED" | cut -d' ' -f1)

if [[ $CHECK -eq 1 ]]; then
    printf '==> Comparing with %s\n' "$REPO_DOCUMENT_REL"
    if [[ ! -f $REPO_DOCUMENT ]]; then
        fail "$REPO_DOCUMENT_REL does not exist: THIRD-PARTY-LICENSES.txt must be regenerated (run build-aux/rust-licenses/generate.sh without --check, then commit $REPO_DOCUMENT_REL)"
    fi
    if ! cmp -s -- "$GENERATED" "$REPO_DOCUMENT"; then
        printf 'generate.sh: %s is STALE: it is not what Cargo.lock, about.toml and the template generate now.\n' "$REPO_DOCUMENT_REL" >&2
        printf '    %s  generated now\n    %s  %s\n' "$generated_sha256" \
            "$(sha256sum -- "$REPO_DOCUMENT" | cut -d' ' -f1)" "$REPO_DOCUMENT_REL" >&2
        printf 'First differences (- repository, + generated):\n' >&2
        diff -u --label "$REPO_DOCUMENT_REL" --label generated -- "$REPO_DOCUMENT" "$GENERATED" \
            | sed -n '3,40p' | sed 's/^/    /' >&2 || true
        fail "THIRD-PARTY-LICENSES.txt must be regenerated: run build-aux/rust-licenses/generate.sh (without --check) in the AppImage build container, review the change, and commit $REPO_DOCUMENT_REL"
    fi
    printf '    %s is current (%s)\n' "$REPO_DOCUMENT_REL" "$generated_sha256"
else
    if [[ -f $REPO_DOCUMENT ]] && cmp -s -- "$GENERATED" "$REPO_DOCUMENT"; then
        printf '==> %s is already current (%s)\n' "$REPO_DOCUMENT_REL" "$generated_sha256"
    else
        install -m644 -- "$GENERATED" "$REPO_DOCUMENT.partial"
        mv -- "$REPO_DOCUMENT.partial" "$REPO_DOCUMENT"
        printf '==> Wrote %s (%s): review the change and commit it\n' "$REPO_DOCUMENT_REL" "$generated_sha256"
    fi
fi

if [[ -n $OUTPUT ]]; then
    install -m644 -- "$GENERATED" "$OUTPUT"
    printf '==> %s\n' "$OUTPUT"
fi
if [[ -n $MANIFEST ]]; then
    install -m644 -- "$WORK/rust-license-manifest.txt" "$MANIFEST"
    printf '==> %s\n' "$MANIFEST"
fi
