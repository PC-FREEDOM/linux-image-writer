#!/usr/bin/env bash
# Builds every artifact of a Linux Image Writer release, from one commit, and
# checks that they all trace back to it. See README.md.
#
#   release.sh --release VERSION   official release: the annotated tag
#                                  vVERSION must point at HEAD, the working
#                                  tree must be clean, and Cargo.toml,
#                                  Cargo.lock and the MetaInfo must say
#                                  VERSION; otherwise nothing is built
#   release.sh --test              test build of HEAD and the working tree,
#                                  named "<Cargo.toml version>-test"
#   release.sh --release VERSION --validate-only
#                                  only the checks (e.g. before tagging, or
#                                  as a CI preflight); builds nothing
#
# Run in the AppImage build container, from the repository root:
#
#   podman run --rm --security-opt label=disable \
#     -v "$PWD":/src \
#     -v linux-image-writer-cargo-registry:/opt/cargo/registry \
#     linux-image-writer-appimage-build \
#     build-aux/release/release.sh --release 0.1.1
#
# It only orchestrates; every artifact is made by the existing scripts:
#   build-aux/rust-licenses/generate.sh --check   the licence document is current
#   build-aux/appimage/build.sh                   the AppImage
#   build-aux/appimage/sources.sh                 its corresponding source archive
#   git archive | gzip -n                         the application's source archive
# with SOURCE_DATE_EPOCH set to the commit's own time, so the same commit gives
# the same bytes. The result is build-aux/release/out/LinuxImageWriter-<label>/:
# the release assets with their .sha256 files, RELEASE-MANIFEST.txt, and the
# build records (manifests) in records/.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
ROOT_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
# For testing the checks only: the repository to check (with --validate-only).
if [[ -n ${LIW_RELEASE_ROOT:-} ]]; then
    ROOT_DIR=$(cd -- "$LIW_RELEASE_ROOT" && pwd -P)
fi
readonly SCRIPT_DIR ROOT_DIR
readonly APPIMAGE_DIR=$ROOT_DIR/build-aux/appimage
readonly APPIMAGE_OUT=$APPIMAGE_DIR/out
readonly OUT_DIR=$SCRIPT_DIR/out
readonly APP_ID=io.github.pc_freedom.linux-image-writer
readonly METAINFO=data/$APP_ID.metainfo.xml

log() { printf '==> %s\n' "$*"; }
fail() { printf 'release.sh: error: %s\n' "$*" >&2; exit 1; }
usage() { printf 'usage: %s --release VERSION [--validate-only] | --test\n' "${0##*/}" >&2; exit 2; }

# The repository mounted into the container belongs to another user id.
git() { command git -c safe.directory="$ROOT_DIR" -C "$ROOT_DIR" "$@"; }

MODE='' VERSION='' VALIDATE_ONLY=0
case ${1:-} in
    --release)
        [[ $# -eq 2 || ( $# -eq 3 && $3 == --validate-only ) ]] || usage
        MODE=release VERSION=$2
        [[ $# -eq 3 ]] && VALIDATE_ONLY=1 ;;
    --test) [[ $# -eq 1 ]] || usage; MODE=test ;;
    *) usage ;;
esac
[[ -z ${LIW_RELEASE_ROOT:-} || $VALIDATE_ONLY == 1 ]] || fail "LIW_RELEASE_ROOT is for --validate-only tests"

WORK=
STAGE=
cleanup() {
    local status=$?
    [[ -z $WORK ]] || rm -rf -- "$WORK"
    if [[ $status -ne 0 ]]; then
        [[ -z $STAGE ]] || rm -rf -- "$STAGE"
        printf 'release.sh: FAILED (exit %d); no release directory was produced.\n' "$status" >&2
    fi
}
trap cleanup EXIT

check_environment() {
    [[ -e /run/.containerenv || -e /.dockerenv ]] \
        || fail "not in a container: run this in the AppImage build container (README.md)"
    local tool
    for tool in git gzip tar zstd sha256sum cmp sed awk; do
        command -v "$tool" >/dev/null || fail "missing tool: $tool"
    done
    git rev-parse --git-dir >/dev/null 2>&1 || fail "$ROOT_DIR is not a git repository"
}

cargo_version() { sed -n 's/^version = "\(.*\)"$/\1/p' -- "$ROOT_DIR/Cargo.toml" | head -n 1; }

# The version of the app's own package in Cargo.lock.
lock_version() {
    awk '$0 == "name = \"linux-image-writer\"" { getline; gsub(/^version = "|"$/, ""); print; exit }' "$ROOT_DIR/Cargo.lock"
}

# The newest <release> of the MetaInfo (the first one listed).
metainfo_version() {
    sed -n 's/.*<release version="\([^"]*\)".*/\1/p' -- "$ROOT_DIR/$METAINFO" | head -n 1
}

# ---- Version and git state ----

COMMIT='' TAG=- LABEL='' REF='' EPOCH='' WORKTREE=clean
validate() {
    COMMIT=$(git rev-parse --verify 'HEAD^{commit}')
    local cargo dirty
    cargo=$(cargo_version)
    dirty=$(git status --porcelain=v1 --untracked-files=normal)
    [[ -z $dirty ]] || WORKTREE=modified

    if [[ $MODE == release ]]; then
        log "Checking the release $VERSION"
        [[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "VERSION must be MAJOR.MINOR.PATCH, got '$VERSION'"
        TAG=v$VERSION
        [[ $WORKTREE == clean ]] \
            || fail "the working tree is not clean (commit or remove these first):"$'\n'"$dirty"
        git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || fail "no tag $TAG"
        [[ $(git cat-file -t "refs/tags/$TAG") == tag ]] || fail "$TAG must be an annotated tag (git tag -a)"
        [[ $(git rev-parse "$TAG^{commit}") == "$COMMIT" ]] \
            || fail "$TAG points at $(git rev-parse --short "$TAG^{commit}"), but HEAD is $(git rev-parse --short HEAD): check out $TAG"
        local others
        others=$(git tag --points-at HEAD | grep -vxF -- "$TAG" || true)
        [[ -z $others ]] \
            || fail "HEAD also carries the tag(s) $(paste -sd' ' <<<"$others"): a release is built from its own commit, never another release's"
        [[ $cargo == "$VERSION" ]] || fail "Cargo.toml says version $cargo, not $VERSION"
        [[ $(lock_version) == "$VERSION" ]] || fail "Cargo.lock says linux-image-writer $(lock_version), not $VERSION (run cargo check after changing Cargo.toml)"
        [[ $(metainfo_version) == "$VERSION" ]] \
            || fail "the newest <release> in $METAINFO is $(metainfo_version), not $VERSION"
        LABEL=$VERSION
        REF=$TAG
    else
        log "Test build of HEAD (no release checks)"
        LABEL=$cargo-test
        REF=HEAD
    fi

    # Every file time in every artifact: the commit's own time.
    EPOCH=$(git show -s --format=%ct "$COMMIT")
    [[ $EPOCH =~ ^[0-9]+$ ]] || fail "cannot read the commit time"
    printf '    commit %s (%s)\n    SOURCE_DATE_EPOCH=%s (%s)\n    label %s, working tree %s\n' \
        "$COMMIT" "$TAG" "$EPOCH" "$(date -u -d "@$EPOCH" +%Y-%m-%dT%H:%M:%SZ)" "$LABEL" "$WORKTREE"
}

# ---- The artifacts, from the existing scripts ----

# The environment of the build scripts: the commit's time; the release
# label and permission only for an official release.
run_build_script() {
    if [[ $MODE == release ]]; then
        env SOURCE_DATE_EPOCH="$EPOCH" APPIMAGE_VERSION="$LABEL" LIW_OFFICIAL_RELEASE=1 "$@"
    else
        env -u APPIMAGE_VERSION -u LIW_OFFICIAL_RELEASE SOURCE_DATE_EPOCH="$EPOCH" "$@"
    fi
}

check_licences() {
    log "Checking data/THIRD-PARTY-LICENSES.txt is current (generate.sh --check)"
    "$ROOT_DIR/build-aux/rust-licenses/generate.sh" --check | sed 's/^/    /'
}

build_appimage() {
    log "Building the AppImage (build-aux/appimage/build.sh)"
    run_build_script "$APPIMAGE_DIR/build.sh" > "$WORK/build.log" 2>&1 \
        || { tail -n 40 -- "$WORK/build.log" >&2; fail "build.sh failed"; }
    grep -E '^==> AppImage ready|^    [0-9a-f]{64}  ' -- "$WORK/build.log" | sed 's/^/    /'
}

build_sources() {
    log "Building the corresponding source archive (build-aux/appimage/sources.sh)"
    run_build_script "$APPIMAGE_DIR/sources.sh" > "$WORK/sources.log" 2>&1 \
        || { tail -n 40 -- "$WORK/sources.log" >&2; fail "sources.sh failed"; }
    grep -E '^==> Source archive ready|^    [0-9a-f]{64}  ' -- "$WORK/sources.log" | sed 's/^/    /'
}

# The application's own source: the tagged tree (HEAD for a test build),
# exactly as committed -- git archive takes tracked files only, so no .git,
# no build output -- with file times set to the commit's time by git and no
# name or time in the gzip header (-n).
SOURCE_ARCHIVE=
build_source_archive() {
    SOURCE_ARCHIVE=LinuxImageWriter-$LABEL-source.tar.gz
    log "Building $SOURCE_ARCHIVE (git archive $REF)"
    git archive --format=tar --prefix="LinuxImageWriter-$LABEL/" "$REF" | gzip -n -9 > "$WORK/$SOURCE_ARCHIVE"
    gzip -t -- "$WORK/$SOURCE_ARCHIVE"
    local list
    list=$(gzip -dc -- "$WORK/$SOURCE_ARCHIVE" | tar -t)
    grep -qxF "LinuxImageWriter-$LABEL/Cargo.lock" <<<"$list" || fail "$SOURCE_ARCHIVE lacks Cargo.lock"
    grep -qxF "LinuxImageWriter-$LABEL/Cargo.toml" <<<"$list" || fail "$SOURCE_ARCHIVE lacks Cargo.toml"
    [[ $(grep -cvE "^LinuxImageWriter-$LABEL/" <<<"$list" || true) == 0 ]] \
        || fail "$SOURCE_ARCHIVE has entries outside LinuxImageWriter-$LABEL/"
    if grep -qE '(^|/)(\.git|target|\.flatpak-builder|build-dir|repo)/|/build-aux/(appimage|release)/out/' <<<"$list"; then
        fail "$SOURCE_ARCHIVE contains build output or repository metadata"
    fi
    local archived
    archived=$(tar_commit "$WORK/$SOURCE_ARCHIVE")
    [[ $archived == "$COMMIT" ]] || fail "$SOURCE_ARCHIVE records commit $archived, not $COMMIT"
    printf '    %s entries, commit %s\n' "$(wc -l <<<"$list")" "$archived"
}

# The commit a git archive tarball records. git get-tar-commit-id reads only
# the first header, so gzip may be cut off (SIGPIPE): that is expected.
tar_commit() {
    { gzip -dc -- "$1" || true; } | git get-tar-commit-id
}

# ---- Assembly and checks ----

ASSETS=()
assemble() {
    local dir=$OUT_DIR/LinuxImageWriter-$LABEL
    STAGE=$dir.partial
    rm -rf -- "$dir" "$STAGE"
    mkdir -p -- "$STAGE/records"

    local appimage=LinuxImageWriter-$LABEL-x86_64.AppImage
    local sources=LinuxImageWriter-$LABEL-appimage-sources.tar.zst
    [[ -f $APPIMAGE_OUT/dist/$appimage ]] || fail "build.sh produced no $appimage"
    [[ -f $APPIMAGE_OUT/dist/$sources ]] || fail "sources.sh produced no $sources"
    install -m755 -- "$APPIMAGE_OUT/dist/$appimage" "$STAGE/$appimage"
    install -m644 -- "$APPIMAGE_OUT/dist/$sources" "$STAGE/$sources"
    install -m644 -- "$WORK/$SOURCE_ARCHIVE" "$STAGE/$SOURCE_ARCHIVE"
    ASSETS=("$appimage" "$SOURCE_ARCHIVE" "$sources")

    local f
    for f in "${ASSETS[@]}"; do
        (cd -- "$STAGE" && sha256sum -- "$f" > "$f.sha256")
    done
    # The build records: not release assets, kept with them.
    install -m644 -- "$APPIMAGE_OUT/bundle-manifest.txt" "$APPIMAGE_OUT/license-manifest.txt" \
        "$APPIMAGE_OUT/rust-license-manifest.txt" "$APPIMAGE_OUT/sources/source-manifest.txt" "$STAGE/records/"
}

# The content of path $1 at the release commit (official) or in the working
# tree (test), for comparing with what the artifacts contain.
committed_file() {
    if [[ $MODE == release ]]; then
        git show "$COMMIT:$1"
    else
        cat -- "$ROOT_DIR/$1"
    fi
}

TRACE_APPIMAGE=
check_traceability() {
    log "Checking every artifact traces back to $COMMIT"
    local appimage=$STAGE/${ASSETS[0]}

    # The AppImage: the app's files in it are the commit's.
    local x=$WORK/extract
    mkdir -p -- "$x"
    (cd -- "$x" && "$appimage" --appimage-extract >/dev/null)
    local root=$x/squashfs-root pair inside source same=0 total=0 head_same=0
    for pair in \
        "AppRun build-aux/appimage/AppRun" \
        "usr/share/applications/$APP_ID.desktop data/$APP_ID.desktop" \
        "usr/share/metainfo/$APP_ID.metainfo.xml $METAINFO" \
        "usr/share/icons/hicolor/scalable/apps/$APP_ID.svg data/icons/hicolor/scalable/apps/$APP_ID.svg" \
        "usr/share/doc/linux-image-writer/LICENSE LICENSE" \
        "usr/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt data/THIRD-PARTY-LICENSES.txt"; do
        inside=${pair%% *} source=${pair#* }
        total=$((total + 1))
        if committed_file "$source" | cmp -s - "$root/$inside"; then
            same=$((same + 1))
        else
            fail "the AppImage's $inside is not $source of the build's tree"
        fi
        git show "$COMMIT:$source" 2>/dev/null | cmp -s - "$root/$inside" && head_same=$((head_same + 1))
    done
    TRACE_APPIMAGE="$same/$total files identical to the tree built; $head_same/$total to the commit"
    printf '    AppImage: %s\n' "$TRACE_APPIMAGE"
    [[ $MODE == test || $head_same == "$total" ]] || fail "the AppImage does not match the commit"

    # The application's source archive: the commit git archive records.
    printf '    source archive: commit %s\n' "$(tar_commit "$STAGE/$SOURCE_ARCHIVE")"

    # The corresponding source archive: built for the Ubuntu snapshot the
    # commit's Containerfile pins, and complete (sources.sh --check).
    local pinned archived
    pinned=$(committed_file build-aux/appimage/Containerfile | sed -n 's/^ARG UBUNTU_SNAPSHOT=//p')
    archived=$(sed -n 's/^ubuntu-snapshot\t//p' -- "$STAGE/records/source-manifest.txt")
    [[ -n $pinned && $pinned == "$archived" ]] || fail "the source archive is for snapshot '$archived', the Containerfile pins '$pinned'"
    printf '    corresponding sources: Ubuntu snapshot %s (as pinned)\n' "$archived"
    "$APPIMAGE_DIR/sources.sh" --check "$STAGE/${ASSETS[2]}" > "$WORK/sources-check.log" 2>&1 \
        || { tail -n 20 -- "$WORK/sources-check.log" >&2; fail "sources.sh --check failed"; }
    printf '    corresponding sources: sources.sh --check passed\n'

    # Exactly the expected files, each matching its .sha256.
    local expected actual f
    expected=$(for f in "${ASSETS[@]}"; do printf '%s\n%s.sha256\n' "$f" "$f"; done | sort)
    actual=$(cd -- "$STAGE" && find . -maxdepth 1 -type f -printf '%f\n' | sort)
    [[ $expected == "$actual" ]] || fail "unexpected release files:"$'\n'"$actual"
    (cd -- "$STAGE" && for f in "${ASSETS[@]}"; do sha256sum --check --status -- "$f.sha256" || exit 1; done) \
        || fail "a .sha256 file does not match"
    printf '    %d assets, each with a matching .sha256\n' "${#ASSETS[@]}"
}

write_manifest() {
    local m=$STAGE/RELEASE-MANIFEST.txt f
    {
        printf '# Linux Image Writer release manifest (format 1)\n'
        printf '# Written by build-aux/release/release.sh. Every artifact below was built\n'
        printf '# from this commit, with all file times set to its commit time.\n'
        printf '\nkind\t%s\n' "$( [[ $MODE == release ]] && printf 'official release' || printf 'TEST build (not for publishing)')"
        printf 'version\t%s\n' "$LABEL"
        printf 'tag\t%s\n' "$TAG"
        printf 'commit\t%s\n' "$COMMIT"
        printf 'source-date-epoch\t%s\t%s\n' "$EPOCH" "$(date -u -d "@$EPOCH" +%Y-%m-%dT%H:%M:%SZ)"
        printf 'working-tree\t%s\n' "$WORKTREE"
        [[ $MODE == test && $WORKTREE == modified ]] \
            && printf 'note\tthe AppImage and corresponding sources were built from the working tree (uncommitted changes); the source archive is the commit only\n'
        printf 'appimage-trace\t%s\n' "$TRACE_APPIMAGE"
        printf '\n[assets]\n'
        printf '# file\tsize\tsha256\n'
        for f in "${ASSETS[@]}"; do
            printf '%s\t%s\t%s\n' "$f" "$(stat -c %s -- "$STAGE/$f")" "$(sha256sum -- "$STAGE/$f" | cut -d' ' -f1)"
        done
        printf '\n[records]\n'
        printf '# Build records: not release assets (README.md).\n'
        printf '# file\tsha256\n'
        for f in bundle-manifest.txt license-manifest.txt rust-license-manifest.txt source-manifest.txt; do
            printf 'records/%s\t%s\n' "$f" "$(sha256sum -- "$STAGE/records/$f" | cut -d' ' -f1)"
        done
    } > "$m"
}

finish() {
    local dir=$OUT_DIR/LinuxImageWriter-$LABEL
    mv -- "$STAGE" "$dir"
    STAGE=
    log "Release directory ready: ${dir#"$ROOT_DIR"/}"
    sed -n '/^\[assets\]/,/^$/p' -- "$dir/RELEASE-MANIFEST.txt" | grep -v '^#' | sed '/^$/d; s/^/    /'
    [[ $MODE == release ]] \
        && printf '    upload exactly the %d assets and their .sha256 files (README.md)\n' "${#ASSETS[@]}" \
        || printf '    a TEST build: not for publishing\n'
}

main() {
    check_environment
    validate
    if [[ $VALIDATE_ONLY == 1 ]]; then
        log "All release checks passed for $LABEL ($COMMIT); nothing built (--validate-only)"
        return 0
    fi
    WORK=$(mktemp -d)
    check_licences
    build_appimage
    build_sources
    build_source_archive
    assemble
    check_traceability
    write_manifest
    finish
}

main "$@"
