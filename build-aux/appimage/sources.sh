#!/usr/bin/env bash
# Builds the AppImage's third-party source archive: the exact Ubuntu source
# packages of every bundled package whose licences need its corresponding
# source carried (the "source-required" packages of out/license-manifest.txt,
# classified with license-review.txt). Run in the AppImage build container,
# after build.sh:
#
#   podman run --rm --security-opt label=disable \
#     -v "$PWD":/src \
#     linux-image-writer-appimage-build \
#     build-aux/appimage/sources.sh
#
# Output (not in Git):
#   out/sources/<source package>/   each source package as Ubuntu publishes
#                                   it (.dsc, .orig.tar.*, .debian.tar.*, ...)
#   out/sources/source-manifest.txt what is there, for which bundled packages
#   out/dist/LinuxImageWriter-<version>-appimage-sources.tar.zst (+ .sha256)
#
# Every source package is the version the bundled binary package was built
# from, fetched from the same Ubuntu snapshot as the build environment. Its
# .dsc must match the snapshot's signed Sources index, and every file the
# .dsc lists must match the .dsc's SHA-256 -- checked on every run, also for
# files downloaded before (out/source-cache/). The archive is reproducible:
# fixed order, times and owners. It is then extracted and checked against
# the manifest: nothing missing, nothing extra, every checksum right.
#
# `sources.sh --check ARCHIVE` only checks an existing archive the same way
# (e.g. before publishing it).
#
# Fetches source packages only; changes nothing on the host.

set -euo pipefail
shopt -s nullglob
export LC_ALL=C

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
ROOT_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly SCRIPT_DIR ROOT_DIR
readonly OUT_DIR=$SCRIPT_DIR/out
readonly LICENSE_MANIFEST=$OUT_DIR/license-manifest.txt
readonly CACHE_DIR=$OUT_DIR/source-cache
readonly SOURCES_DIR=$OUT_DIR/sources
readonly SOURCE_MANIFEST=$SOURCES_DIR/source-manifest.txt
readonly DIST_DIR=$OUT_DIR/dist
readonly CHECK_DIR=$OUT_DIR/sources-check

log() { printf '==> %s\n' "$*"; }
fail() { printf 'sources.sh: error: %s\n' "$*" >&2; exit 1; }

ARCHIVE=
BUILDING=
cleanup() {
    local status=$?
    rm -rf -- "$CHECK_DIR"
    if [[ $status -ne 0 && -n $BUILDING ]]; then
        rm -rf -- "$SOURCES_DIR"
        [[ -n $ARCHIVE ]] && rm -f -- "$ARCHIVE.partial"
        printf 'sources.sh: FAILED (exit %d)\n' "$status" >&2
    fi
}
trap cleanup EXIT

# ---- Environment ----

SNAPSHOT=
check_environment() {
    [[ -e /run/.containerenv || -e /.dockerenv ]] \
        || fail "not in a container: run this in the image built from build-aux/appimage/Containerfile"
    local tool
    for tool in apt-get apt-cache dpkg-query sha256sum tar zstd; do
        command -v "$tool" >/dev/null || fail "missing tool: $tool"
    done
    # Refused before anything is fetched (see build_archive).
    if [[ -n ${APPIMAGE_VERSION:-} && $APPIMAGE_VERSION != *-test && ${LIW_OFFICIAL_RELEASE:-} != 1 ]]; then
        fail "APPIMAGE_VERSION=$APPIMAGE_VERSION names a release; official artifacts are built only by build-aux/release/release.sh (use a label ending in -test for a test build)"
    fi
    [[ -s $LICENSE_MANIFEST ]] || fail "no ${LICENSE_MANIFEST#"$ROOT_DIR"/}: run build.sh first"
    SNAPSHOT=$(apt-config dump | sed -n 's/^APT::Snapshot "\([0-9]\{8\}T[0-9]\{6\}Z\)";$/\1/p')
    [[ -n $SNAPSHOT ]] || fail "no Ubuntu snapshot configured"
    local manifest_snapshot
    manifest_snapshot=$(sed -n 's/^ubuntu-snapshot\t//p' -- "$LICENSE_MANIFEST")
    [[ $manifest_snapshot == "$SNAPSHOT" ]] \
        || fail "the licence manifest is from snapshot $manifest_snapshot, this environment is $SNAPSHOT: run build.sh again"
}

# ---- What is needed ----

# source package -> its version, bundled binary packages, classes
declare -A SRC_VERSION=() SRC_BINARIES=() SRC_CLASSES=() SRC_FILES=()

read_targets() {
    log "Reading the source-required packages from ${LICENSE_MANIFEST#"$ROOT_DIR"/}"
    local pkg version src srcver classes required _l _d _s _o files
    local count=0
    while IFS=$'\t' read -r pkg version src srcver classes required _l _d _s _o files; do
        [[ $required == yes ]] || continue
        # The binary package installed in this environment is the one bundled.
        [[ $(dpkg-query -W -f='${Version}\t${source:Package}\t${source:Version}' -- "$pkg") == "$version"$'\t'"$src"$'\t'"$srcver" ]] \
            || fail "$pkg: the installed package is not $version from $src $srcver"
        if [[ -n ${SRC_VERSION[$src]:-} && ${SRC_VERSION[$src]} != "$srcver" ]]; then
            fail "$src: two versions needed (${SRC_VERSION[$src]} and $srcver)"
        fi
        SRC_VERSION[$src]=$srcver
        SRC_BINARIES[$src]+="$pkg=$version "
        SRC_CLASSES[$src]+="${classes//,/ } "
        SRC_FILES[$src]+="${files//,/ } "
        count=$((count + 1))
    done < <(awk '/^\[packages\]/ { f = 1; next } /^\[/ { f = 0 } f && !/^#/ && NF' "$LICENSE_MANIFEST")
    [[ $count -gt 0 ]] || fail "no source-required package in the licence manifest"
    printf '    %d binary packages, from %d source packages\n' "$count" "${#SRC_VERSION[@]}"
}

# ---- Fetching and verifying ----

enable_source_index() {
    log "Enabling the snapshot's source index (deb-src, in this container only)"
    sed -e 's/^Types: deb$/Types: deb-src/' /etc/apt/sources.list.d/ubuntu.sources \
        > /etc/apt/sources.list.d/ubuntu-src.sources
    apt-get update -qq >/dev/null
}

# The SHA-256 the snapshot's (signed) Sources index gives for a source
# package version's .dsc.
indexed_dsc_sha256() {
    local src=$1 ver=$2
    apt-cache showsrc --only-source "$src" 2>/dev/null | awk -v ver="$ver" '
        /^Version: / { v = $2 }
        /^Checksums-Sha256:/ { c = (v == ver); next }
        /^[^ ]/ { c = 0 }
        c && $3 ~ /\.dsc$/ { print $1; exit }'
}

# The files a .dsc lists: "sha256 size name" lines.
dsc_files() {
    awk '/^Checksums-Sha256:/ { c = 1; next } /^[^ ]/ { c = 0 } c && NF == 3 { print $1, $2, $3 }' "$1"
}

# Checks that DIR holds exactly source package SRC version VER: its .dsc as
# indexed, and every file it lists with the right size and SHA-256.
verify_source_dir() {
    local dir=$1 src=$2 ver=$3
    local dsc=$dir/${src}_${ver#*:}.dsc
    [[ -f $dsc ]] || { printf 'no %s\n' "${dsc##*/}"; return 1; }
    local want
    want=$(indexed_dsc_sha256 "$src" "$ver")
    [[ -n $want ]] || { printf 'the snapshot index has no %s %s\n' "$src" "$ver"; return 1; }
    [[ $(sha256sum -- "$dsc" | cut -d' ' -f1) == "$want" ]] || { printf '%s does not match the index\n' "${dsc##*/}"; return 1; }
    grep -qx "Source: $src" -- "$dsc" && grep -qx "Version: $ver" -- "$dsc" \
        || { printf '%s is not %s %s\n' "${dsc##*/}" "$src" "$ver"; return 1; }
    local sha size name expected=("${dsc##*/}")
    while read -r sha size name; do
        expected+=("$name")
        [[ -f $dir/$name ]] || { printf 'missing %s\n' "$name"; return 1; }
        [[ $(stat -c %s -- "$dir/$name") == "$size" ]] || { printf '%s: wrong size\n' "$name"; return 1; }
        [[ $(sha256sum -- "$dir/$name" | cut -d' ' -f1) == "$sha" ]] || { printf '%s: SHA-256 differs from the .dsc\n' "$name"; return 1; }
    done < <(dsc_files "$dsc")
    [[ ${#expected[@]} -ge 2 ]] || { printf '%s lists no files\n' "${dsc##*/}"; return 1; }
    local present
    present=$(cd -- "$dir" && find . -mindepth 1 -printf '%P\n' | sort)
    [[ $present == "$(printf '%s\n' "${expected[@]}" | sort)" ]] \
        || { printf 'unexpected files: %s\n' "$(comm -23 <(printf '%s\n' "$present") <(printf '%s\n' "${expected[@]}" | sort) | paste -sd' ' -)"; return 1; }
}

fetch_sources() {
    log "Fetching and verifying the source packages (snapshot $SNAPSHOT)"
    mkdir -p -- "$CACHE_DIR"
    local src ver dir problem fetched=0 reused=0
    for src in $(printf '%s\n' "${!SRC_VERSION[@]}" | sort); do
        ver=${SRC_VERSION[$src]}
        dir=$CACHE_DIR/${src}_${ver#*:}
        if [[ -d $dir ]] && verify_source_dir "$dir" "$src" "$ver" >/dev/null; then
            reused=$((reused + 1))
            continue
        fi
        rm -rf -- "$dir" "$dir.download"
        mkdir -p -- "$dir.download"
        (cd -- "$dir.download" && apt-get source -qq --download-only --only-source "$src=$ver" >/dev/null 2>&1) \
            || fail "apt-get source $src=$ver failed"
        problem=$(verify_source_dir "$dir.download" "$src" "$ver") || fail "$src $ver: $problem"
        mv -- "$dir.download" "$dir"
        fetched=$((fetched + 1))
    done
    printf '    %d source packages verified (%d downloaded, %d reused from out/source-cache)\n' \
        "${#SRC_VERSION[@]}" "$fetched" "$reused"
}

# ---- The source tree and its manifest ----

assemble_sources() {
    log "Assembling ${SOURCES_DIR#"$ROOT_DIR"/}"
    rm -rf -- "$SOURCES_DIR"
    mkdir -p -- "$SOURCES_DIR"
    local src ver
    for src in "${!SRC_VERSION[@]}"; do
        ver=${SRC_VERSION[$src]}
        cp -r -- "$CACHE_DIR/${src}_${ver#*:}" "$SOURCES_DIR/$src"
    done
    find "$SOURCES_DIR" -type d -exec chmod 755 {} +
    find "$SOURCES_DIR" -type f -exec chmod 644 {} +
    write_source_manifest
}

write_source_manifest() {
    local src ver dsc directory
    {
        printf '# Linux Image Writer AppImage -- third-party source manifest (format 1)\n'
        printf '# The corresponding source of every bundled Ubuntu package whose licence\n'
        printf '# asks for it, exactly as Ubuntu published it (source package directories\n'
        printf '# beside this file). Written by build-aux/appimage/sources.sh. Tab-separated.\n'
        printf '\n[inputs]\n'
        # shellcheck source=/dev/null
        printf 'os\t%s\n' "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")"
        printf 'ubuntu-snapshot\t%s\n' "$SNAPSHOT"
        printf 'source-packages\t%d\n' "${#SRC_VERSION[@]}"
        printf '\n[sources]\n'
        printf '# source-package\tsource-version\tbinary-packages (bundled)\tlicence classes\tbundled files\n'
        for src in "${!SRC_VERSION[@]}"; do
            printf '%s\t%s\t%s\t%s\t%s\n' "$src" "${SRC_VERSION[$src]}" \
                "$(tr ' ' '\n' <<<"${SRC_BINARIES[$src]}" | sed '/^$/d' | sort -u | paste -sd, -)" \
                "$(tr ' ' '\n' <<<"${SRC_CLASSES[$src]}" | sed '/^$/d' | sort -u | paste -sd, -)" \
                "$(tr ' ' '\n' <<<"${SRC_FILES[$src]}" | sed '/^$/d' | sort -u | paste -sd, -)"
        done | sort
        printf '\n[files]\n'
        printf '# sha256\tsize\tpath (relative to this file)\n'
        (cd -- "$SOURCES_DIR" && find . -mindepth 2 -type f -printf '%P\n' | sort | while IFS= read -r f; do
            printf '%s\t%s\t%s\n' "$(sha256sum -- "$f" | cut -d' ' -f1)" "$(stat -c %s -- "$f")" "$f"
        done)
        printf '\n[provenance]\n'
        printf '# Where each source package was fetched from: the Ubuntu Snapshot Service,\n'
        printf '# at the snapshot the AppImage was built from (for reference; the files above\n'
        printf '# are the source this release provides).\n'
        for src in "${!SRC_VERSION[@]}"; do
            ver=${SRC_VERSION[$src]}
            directory=$(apt-cache showsrc --only-source "$src" | awk -v ver="$ver" '/^Version: / { v = $2 } /^Directory: / && v == ver { print $2; exit }')
            printf '%s\t%s\thttps://snapshot.ubuntu.com/ubuntu/%s/%s/\n' "$src" "$ver" "$SNAPSHOT" "$directory"
        done | sort
    } > "$SOURCE_MANIFEST"
}

# ---- The archive ----

# File times in the archive: SOURCE_DATE_EPOCH, else the Ubuntu snapshot's
# time (as for the AppImage).
archive_epoch() {
    if [[ -n ${SOURCE_DATE_EPOCH:-} ]]; then
        [[ $SOURCE_DATE_EPOCH =~ ^[0-9]+$ ]] || fail "SOURCE_DATE_EPOCH must be a number of seconds"
        printf '%s' "$SOURCE_DATE_EPOCH"
    else
        date -u -d "${SNAPSHOT:0:4}-${SNAPSHOT:4:2}-${SNAPSHOT:6:2}T${SNAPSHOT:9:2}:${SNAPSHOT:11:2}:${SNAPSHOT:13:2}Z" +%s
    fi
}

build_archive() {
    local version label epoch
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' -- "$ROOT_DIR/Cargo.toml" | head -n 1)
    label=${APPIMAGE_VERSION:-$version-test}
    [[ $label =~ ^[0-9A-Za-z.+_-]+$ ]] || fail "unusable version label: $label"
    # A name without "-test" is a release's: only build-aux/release/release.sh,
    # after checking the tag, the commit and the version, may produce one.
    if [[ $label != *-test && ${LIW_OFFICIAL_RELEASE:-} != 1 ]]; then
        fail "APPIMAGE_VERSION=$label names a release; official artifacts are built only by build-aux/release/release.sh (use a label ending in -test for a test build)"
    fi
    ARCHIVE=$DIST_DIR/LinuxImageWriter-$label-appimage-sources.tar.zst
    epoch=$(archive_epoch)
    log "Building ${ARCHIVE#"$ROOT_DIR"/}"
    mkdir -p -- "$DIST_DIR"
    tar --create --directory="$OUT_DIR" --sort=name --format=posix \
        --pax-option='exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime' \
        --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
        sources \
        | zstd -q -19 -T1 --no-check > "$ARCHIVE.partial"
}

# ---- Checks on the archive ----

# check_archive FILE [MANIFEST]: FILE holds exactly the needed source
# packages; if MANIFEST is given, its manifest must be that file.
check_archive() {
    local archive=$1 expected_manifest=${2:-}
    log "Checking ${archive#"$ROOT_DIR"/}"
    rm -rf -- "$CHECK_DIR"
    mkdir -p -- "$CHECK_DIR"
    zstd -q -d -c -- "$archive" | tar --extract --directory="$CHECK_DIR" || fail "cannot extract the archive"
    local root=$CHECK_DIR/sources manifest=$CHECK_DIR/sources/source-manifest.txt
    [[ -f $manifest ]] || fail "the archive has no sources/source-manifest.txt"

    # Exactly the files the manifest lists, plus the manifest.
    local listed present
    listed=$(awk -F'\t' '/^\[files\]/ { f = 1; next } /^\[/ { f = 0 } f && !/^#/ && NF { print $3 }' "$manifest" | sort)
    present=$(cd -- "$root" && find . -type f -printf '%P\n' | grep -vx 'source-manifest.txt' | sort)
    [[ $listed == "$present" ]] || fail "the archive's files differ from its manifest: $(diff <(printf '%s\n' "$listed") <(printf '%s\n' "$present") | grep '^[<>]' | head -5 | paste -sd' ' -)"
    # Every listed file with its SHA-256.
    (cd -- "$root" && awk -F'\t' '/^\[files\]/ { f = 1; next } /^\[/ { f = 0 } f && !/^#/ && NF { print $1 "  " $3 }' source-manifest.txt \
        | sha256sum --check --quiet) || fail "a file in the archive does not match its SHA-256 in the manifest"
    # The versions it names are the needed ones.
    local src dirs
    [[ $(awk -F'\t' '/^\[sources\]/ { f = 1; next } /^\[/ { f = 0 } f && !/^#/ && NF { print $1 "=" $2 }' "$manifest" | sort) \
       == "$(for src in "${!SRC_VERSION[@]}"; do printf '%s=%s\n' "$src" "${SRC_VERSION[$src]}"; done | sort)" ]] \
        || fail "the archive's manifest names other source packages or versions than the bundled ones need"
    # Every needed source package, as its .dsc says; no other directory.
    dirs=$(cd -- "$root" && find . -mindepth 1 -maxdepth 1 -type d -printf '%P\n' | sort)
    [[ $dirs == "$(printf '%s\n' "${!SRC_VERSION[@]}" | sort)" ]] \
        || fail "the archive's source packages differ from the source-required ones"
    for src in "${!SRC_VERSION[@]}"; do
        local problem
        problem=$(verify_source_dir "$root/$src" "$src" "${SRC_VERSION[$src]}") || fail "archive: $src: $problem"
    done
    if [[ -n $expected_manifest ]]; then
        cmp -s -- "$manifest" "$expected_manifest" || fail "the archive's manifest differs from ${expected_manifest#"$ROOT_DIR"/}"
    fi
    rm -rf -- "$CHECK_DIR"
    printf '    %d source packages, %d files: all present, verified against their .dsc and the manifest\n' \
        "${#SRC_VERSION[@]}" "$(printf '%s\n' "$listed" | grep -c .)"
}

finish() {
    mv -- "$ARCHIVE.partial" "$ARCHIVE"
    (cd -- "$DIST_DIR" && sha256sum -- "${ARCHIVE##*/}" > "${ARCHIVE##*/}.sha256")
    log "Source archive ready: ${ARCHIVE#"$ROOT_DIR"/}"
    printf '    %s\n' "$(cat -- "$ARCHIVE.sha256")"
    printf '    %d bytes; manifest: %s\n' "$(stat -c %s -- "$ARCHIVE")" "${SOURCE_MANIFEST#"$ROOT_DIR"/}"
    [[ $ARCHIVE == *-test-* ]] && printf '    a TEST build: not for publishing\n'
    return 0
}

main() {
    check_environment
    read_targets
    enable_source_index
    if [[ ${1:-} == --check ]]; then
        [[ -f ${2:-} ]] || fail "usage: sources.sh --check ARCHIVE"
        check_archive "$(cd -- "$(dirname -- "$2")" && pwd -P)/${2##*/}"
        printf 'sources.sh: PASS\n'
        return
    fi
    BUILDING=1
    fetch_sources
    assemble_sources
    build_archive
    check_archive "$ARCHIVE.partial" "$SOURCE_MANIFEST"
    finish
}

main "$@"
