#!/usr/bin/env bash
# Builds the Linux Image Writer AppDir, from scratch, inside the AppImage
# build container (Containerfile). See README.md.
#
#   podman run --rm --security-opt label=disable \
#     -v "$PWD":/src \
#     -v linux-image-writer-cargo-registry:/opt/cargo/registry \
#     linux-image-writer-appimage-build \
#     build-aux/appimage/build.sh
#
# Stages (each a function below, run in order by main):
#   0. check_icon_list    icons.txt names exactly the icons the GUI uses
#   1. build_binary       the GUI, with cargo, in this container
#      generate_rust_licenses  regenerates THIRD-PARTY-LICENSES.txt, the
#                         licences of the Rust crates compiled into it,
#                         checks it against the dependency graph and the
#                         binary (build-aux/rust-licenses/, shared with the
#                         Flatpak), and stops the build unless the copy the
#                         AppImage ships, data/THIRD-PARTY-LICENSES.txt, is
#                         identical to it
#   2. assemble_appdir    the binary and the existing desktop entry, MetaInfo,
#                         icon and translations (data/ and po/ are the source
#                         of truth; nothing is copied into the source tree)
#   3. check_binary,      checks against the sources, and validation
#      check_app_files
#   4. fetch_linuxdeploy  linuxdeploy at the release tools.lock pins, verified
#                         by SHA-256
#   5. deploy_libraries   linuxdeploy collects the libraries the binary and
#                         the gdk-pixbuf SVG loader need into usr/lib,
#                         leaving out exclude-libs.txt
#   6. add_gtk_resources  what GTK needs at run time besides libraries: its
#                         FileChooser schema, the SVG loader's cache, an
#                         empty GIO module directory, and the icons GTK and
#                         libadwaita lack
#   7. install_apprun     AppRun, the launcher
#      add_license_documents  the copyright file of every bundled Ubuntu
#                         package, the app's own licence, and the Rust
#                         crates' licence document
#   8. check_app_files    again: the app's own files must be untouched
#      check_bundle       check-appdir.sh: every ELF file, the resources, and
#                         the bundle manifest (out/bundle-manifest.txt)
#   9. finish             the AppDir is complete: out/LinuxImageWriter.AppDir
#  10. build_appimage     appimagetool, with the pinned type 2 runtime, packs
#                         it into out/dist/ (reproducibly: fixed timestamps
#                         and owners)
#  11. check_appimage     the AppImage's runtime, and its extracted contents
#                         against the AppDir; then its SHA-256 file
#
# The previous AppDir is removed first, and the new one is assembled under a
# temporary name and only renamed to out/LinuxImageWriter.AppDir after every
# check has passed: a failed build never leaves something that looks like a
# finished AppDir. The AppImage, likewise, is only given its name after its
# checks.
#
# The AppImage is a test build unless APPIMAGE_VERSION names a release: its
# file name carries "<Cargo version>-test" by default, so it is never mistaken
# for a published one.
#
# Builds only: no tests (run cargo test separately), no devices, no root
# privileges beyond the container's own unprivileged user namespace.

set -euo pipefail
shopt -s nullglob

readonly APP_ID=io.github.pc_freedom.linux-image-writer
readonly BIN_NAME=linux-image-writer
readonly GETTEXT_DOMAIN=linux-image-writer
readonly APPDIR_NAME=LinuxImageWriter.AppDir
# The newest glibc symbol version the AppImage may require (Ubuntu 24.04).
readonly GLIBC_MAX=2.39

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
ROOT_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly SCRIPT_DIR ROOT_DIR
readonly OUT_DIR=$SCRIPT_DIR/out
readonly APPDIR=$OUT_DIR/$APPDIR_NAME
readonly WORK_APPDIR=$OUT_DIR/$APPDIR_NAME.partial
readonly MANIFEST=$OUT_DIR/bundle-manifest.txt
readonly LICENSE_MANIFEST=$OUT_DIR/license-manifest.txt
# The Rust crates' licence document and its manifest (build-aux/rust-licenses).
readonly RUST_LICENSES_DIR=$ROOT_DIR/build-aux/rust-licenses
readonly RUST_DOCUMENT=$OUT_DIR/THIRD-PARTY-LICENSES.txt
readonly RUST_LICENSE_MANIFEST=$OUT_DIR/rust-license-manifest.txt
readonly DIST_DIR=$OUT_DIR/dist
readonly EXTRACT_DIR=$OUT_DIR/extract
readonly LINUXDEPLOY_LOG=$OUT_DIR/linuxdeploy.log
# Downloaded tools, kept between builds and verified on every use.
readonly TOOLS_DIR=$OUT_DIR/tools
readonly TOOLS_LOCK=$SCRIPT_DIR/tools.lock
readonly EXCLUDE_LIST=$SCRIPT_DIR/exclude-libs.txt
readonly ICONS_LIST=$SCRIPT_DIR/icons.txt
readonly APPRUN_SRC=$SCRIPT_DIR/AppRun

# Runtime resources, from the build environment's Ubuntu packages.
readonly MULTIARCH_DIR=/usr/lib/x86_64-linux-gnu
readonly PIXBUF_DIR=$MULTIARCH_DIR/gdk-pixbuf-2.0/2.10.0
readonly PIXBUF_QUERY_LOADERS=$MULTIARCH_DIR/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders
readonly SVG_LOADER=$PIXBUF_DIR/loaders/libpixbufloader-svg.so
readonly SYSTEM_SCHEMA_DIR=/usr/share/glib-2.0/schemas
readonly ADWAITA_SYMBOLIC_DIR=/usr/share/icons/Adwaita/symbolic
readonly HICOLOR_INDEX=/usr/share/icons/hicolor/index.theme
# GTK's own GSettings schemas the GUI needs: the file chooser's, used when
# GTK shows its own file dialog (no file chooser portal). Measured: nothing
# else asks for a schema.
readonly GTK_SCHEMAS=(org.gtk.gtk4.Settings.FileChooser)
# Fixed, so the binary is always the one this container built -- never the
# host's target/release.
readonly CARGO_TARGET_DIR=$ROOT_DIR/target/appimage
export CARGO_TARGET_DIR
readonly BINARY=$CARGO_TARGET_DIR/release/$BIN_NAME

readonly SRC_DESKTOP=$ROOT_DIR/data/$APP_ID.desktop
readonly SRC_METAINFO=$ROOT_DIR/data/$APP_ID.metainfo.xml
readonly SRC_ICON=$ROOT_DIR/data/icons/hicolor/scalable/apps/$APP_ID.svg
readonly SRC_LINGUAS=$ROOT_DIR/po/LINGUAS
# The Rust crates' licence document, as the repository keeps it (generated by
# build-aux/rust-licenses/generate.sh; the AppImage and the Flatpak ship it).
readonly SRC_RUST_DOCUMENT=$ROOT_DIR/data/THIRD-PARTY-LICENSES.txt

log() { printf '==> %s\n' "$*"; }
fail() { printf 'build.sh: error: %s\n' "$*" >&2; exit 1; }

# Removes a partial AppDir when the build stops early. A partial manifest is
# kept, for finding out what was wrong.
cleanup() {
    local status=$?
    if [[ $status -ne 0 ]]; then
        rm -rf -- "$WORK_APPDIR" "$EXTRACT_DIR"
        rm -f -- "$DIST_DIR"/*.partial
        if [[ -d $APPDIR ]]; then
            printf 'build.sh: FAILED (exit %d); the AppDir is complete, but no AppImage was produced.\n' "$status" >&2
        else
            printf 'build.sh: FAILED (exit %d); no AppDir was produced.\n' "$status" >&2
        fi
    fi
}
trap cleanup EXIT

# The languages in po/LINGUAS (one or more per line; '#' starts a comment).
linguas() {
    sed -e 's/#.*//' -- "$SRC_LINGUAS" | tr -s '[:space:]' '\n' | sed '/^$/d'
}

# The value of KEY in tools.lock (read as text, never evaluated).
lock_value() {
    local value
    value=$(sed -n "s/^$1=//p" -- "$TOOLS_LOCK")
    [[ -n $value && $value != *$'\n'* ]] || fail "tools.lock: expected exactly one $1"
    printf '%s' "$value"
}

# The patterns in exclude-libs.txt (first word of each non-comment line).
exclude_patterns() {
    sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$EXCLUDE_LIST" | awk '{ print $1 }'
}

check_environment() {
    # The AppImage is built only in the pinned Ubuntu 24.04 environment, and
    # never on the host (where cargo would link against the host's glibc and
    # GTK). Inside a rootless container, its root user is the invoking user.
    [[ -e /run/.containerenv || -e /.dockerenv ]] \
        || fail "not in a container: run this in the image built from build-aux/appimage/Containerfile (see README.md)"
    local id version
    # shellcheck source=/dev/null
    id=$(. /etc/os-release && printf '%s' "$ID")
    # shellcheck source=/dev/null
    version=$(. /etc/os-release && printf '%s' "$VERSION_ID")
    [[ $id == ubuntu && $version == 24.04 ]] \
        || fail "expected Ubuntu 24.04, found $id $version"

    [[ -f $ROOT_DIR/Cargo.toml && -f $SRC_DESKTOP ]] \
        || fail "repository root not found at $ROOT_DIR"
    grep -qx 'name = "linux-image-writer"' "$ROOT_DIR/Cargo.toml" \
        || fail "$ROOT_DIR/Cargo.toml is not linux-image-writer's"

    local tool
    for tool in cargo cargo-about python3 msgfmt desktop-file-validate appstreamcli file objdump readelf cmp sha256sum curl dpkg glib-compile-schemas strings rsvg-convert od cmp diff; do
        command -v "$tool" >/dev/null || fail "missing tool: $tool"
    done
    [[ -x $PIXBUF_QUERY_LOADERS && -f $SVG_LOADER ]] || fail "gdk-pixbuf's query tool or SVG loader is missing"

    # Refused here already, before anything is built (see build_appimage).
    if [[ -n ${APPIMAGE_VERSION:-} && $APPIMAGE_VERSION != *-test && ${LIW_OFFICIAL_RELEASE:-} != 1 ]]; then
        fail "APPIMAGE_VERSION=$APPIMAGE_VERSION names a release; official artifacts are built only by build-aux/release/release.sh (use a label ending in -test for a test build)"
    fi
}

# The entries of icons.txt: "name source [context]" per line.
icon_entries() {
    sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$ICONS_LIST"
}

# ---- 0. Icons ----

# Every symbolic icon name written in the GUI's source must be in icons.txt,
# and icons.txt must list nothing else, so a new icon cannot be forgotten.
check_icon_list() {
    log "Checking icons.txt against the icons the GUI uses"
    local used listed
    used=$(grep -rhoE '"[a-z0-9-]+-symbolic"' -- "$ROOT_DIR/src/bin/linux-image-writer" | tr -d '"' | sort -u)
    listed=$(icon_entries | awk '{ print $1 }' | sort -u)
    local missing extra
    missing=$(comm -23 <(printf '%s\n' "$used") <(printf '%s\n' "$listed"))
    extra=$(comm -13 <(printf '%s\n' "$used") <(printf '%s\n' "$listed"))
    [[ -z $missing ]] || fail "icons used by the GUI but not in icons.txt: $(paste -sd' ' <<<"$missing")"
    [[ -z $extra ]] || fail "icons in icons.txt the GUI no longer uses: $(paste -sd' ' <<<"$extra")"
    local name source context
    while read -r name source context; do
        case $source in
            gtk) [[ -z $context ]] || fail "icons.txt: $name: a gtk icon takes no context" ;;
            adwaita) [[ -f $ADWAITA_SYMBOLIC_DIR/$context/$name.svg ]] \
                || fail "icons.txt: $name: not in Adwaita's $context icons" ;;
            *) fail "icons.txt: $name: unknown source '$source'" ;;
        esac
    done < <(icon_entries)
    printf '    %d icons: %d from GTK / libadwaita, %d from Adwaita\n' \
        "$(icon_entries | wc -l)" "$(icon_entries | awk '$2 == "gtk"' | wc -l)" \
        "$(icon_entries | awk '$2 == "adwaita"' | wc -l)"
}

# The previous AppDir and its reports go before anything is built, so a failed
# build leaves none at all rather than older ones that look current.
clean_output() {
    mkdir -p -- "$OUT_DIR"
    rm -rf -- "$APPDIR" "$WORK_APPDIR" "$DIST_DIR" "$EXTRACT_DIR"
    rm -f -- "$MANIFEST" "$MANIFEST.partial" "$LICENSE_MANIFEST" "$LICENSE_MANIFEST.partial" "$LINUXDEPLOY_LOG" \
        "$RUST_DOCUMENT" "$RUST_DOCUMENT.partial" "$RUST_LICENSE_MANIFEST" "$RUST_LICENSE_MANIFEST.partial"
}

# ---- 1. Binary ----

build_binary() {
    log "Building $BIN_NAME (release, --features gui) into $CARGO_TARGET_DIR"
    (cd -- "$ROOT_DIR" && cargo build --release --locked --features gui --bin "$BIN_NAME")
    [[ -x $BINARY ]] || fail "cargo did not produce $BINARY"
}

# The licences of the Rust crates compiled into the binary, regenerated from
# Cargo.lock by the pinned cargo-about and checked (generate.sh, check.py):
# every distributed crate, and only those, with its licence text. The
# AppImage ships the repository's copy, data/THIRD-PARTY-LICENSES.txt, so it
# must be byte for byte what was just generated: a stale copy (Cargo.lock,
# about.toml, the template or a crate's licence files changed without
# regenerating it) stops the build here. The generated document and the
# manifest are kept under temporary names until the AppDir is complete.
generate_rust_licenses() {
    log "Regenerating the Rust crates' licence document and checking data/THIRD-PARTY-LICENSES.txt"
    "$RUST_LICENSES_DIR/generate.sh" --check --output "$RUST_DOCUMENT.partial" \
        --manifest "$RUST_LICENSE_MANIFEST.partial" --binary "$BINARY" | sed 's/^/    /'
    cmp -s -- "$RUST_DOCUMENT.partial" "$SRC_RUST_DOCUMENT" \
        || fail "${SRC_RUST_DOCUMENT#"$ROOT_DIR"/} differs from the generated document: THIRD-PARTY-LICENSES.txt must be regenerated (build-aux/rust-licenses/generate.sh)"
    printf '    %s: identical to the generated document\n' "${SRC_RUST_DOCUMENT#"$ROOT_DIR"/}"
}

# ---- 2. AppDir ----

assemble_appdir() {
    log "Assembling $APPDIR_NAME"
    mkdir -- "$WORK_APPDIR"

    local usr=$WORK_APPDIR/usr
    install -Dm755 -- "$BINARY" "$usr/bin/$BIN_NAME"
    install -Dm644 -- "$SRC_DESKTOP" "$usr/share/applications/$APP_ID.desktop"
    install -Dm644 -- "$SRC_METAINFO" "$usr/share/metainfo/$APP_ID.metainfo.xml"
    install -Dm644 -- "$SRC_ICON" "$usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"

    # Translations, as the Flatpak manifest compiles them: every language in
    # po/LINGUAS, with msgfmt --check (which includes --check-format).
    local lang dir
    while IFS= read -r lang; do
        [[ -f $ROOT_DIR/po/$lang.po ]] || fail "po/LINGUAS lists $lang, but po/$lang.po does not exist"
        dir=$usr/share/locale/$lang/LC_MESSAGES
        mkdir -p -- "$dir"
        log "Compiling po/$lang.po"
        msgfmt --check --check-format --statistics \
            -o "$dir/$GETTEXT_DOMAIN.mo" -- "$ROOT_DIR/po/$lang.po"
    done < <(linguas)

    # The AppDir's top level: the desktop entry and icon AppImage tools look
    # for, as relative links to the installed copies.
    ln -s -- "usr/share/applications/$APP_ID.desktop" "$WORK_APPDIR/$APP_ID.desktop"
    ln -s -- "usr/share/icons/hicolor/scalable/apps/$APP_ID.svg" "$WORK_APPDIR/$APP_ID.svg"

    # .DirIcon: the AppImage specification requires it and says it should be
    # a 256x256 PNG (thumbnailers and desktop integration tools read it). It
    # is rendered from the SVG icon here; the SVG stays the only source.
    rsvg-convert --width=256 --height=256 --keep-aspect-ratio --format=png \
        --output="$WORK_APPDIR/.DirIcon" -- "$SRC_ICON"
}

# ---- 3. Checks on the app's own files ----

# Fails unless file $1 has the same contents as source $2.
same_as_source() {
    cmp -s -- "$1" "$2" || fail "${1#"$WORK_APPDIR"/} differs from ${2#"$ROOT_DIR"/}"
    printf '    same as %s  %s\n' "${2#"$ROOT_DIR"/}" "$(sha256sum -- "$2" | cut -d' ' -f1)"
}

# The binary as cargo built it. Runs before linuxdeploy, which adds a RUNPATH
# to it (check-appdir.sh checks it after that).
check_binary() {
    local bin=$WORK_APPDIR/usr/bin/$BIN_NAME

    log "Checking the binary"
    same_as_source "$bin" "$BINARY"
    local elf
    elf=$(file -b -- "$bin")
    [[ $elf == *"ELF 64-bit"*"x86-64"* ]] || fail "unexpected binary: $elf"
    # The C runtime objects linked into it carry the compiler's version: the
    # binary was linked in this Ubuntu 24.04 environment.
    local comment
    comment=$(readelf -p .comment -- "$bin" | grep -o 'GCC: (Ubuntu[^)]*24\.04[^)]*) [0-9.]*' | head -n1 || true)
    [[ -n $comment ]] || fail "the binary was not linked by Ubuntu 24.04's toolchain"
    printf '    linked by: %s\n' "$comment"
}

# The desktop entry, MetaInfo, icon, links and translations: identical to
# their sources, and valid. Runs before and after linuxdeploy.
check_app_files() {
    local usr=$WORK_APPDIR/usr

    log "Checking the app's files against their sources"
    same_as_source "$usr/share/applications/$APP_ID.desktop" "$SRC_DESKTOP"
    same_as_source "$usr/share/metainfo/$APP_ID.metainfo.xml" "$SRC_METAINFO"
    same_as_source "$usr/share/icons/hicolor/scalable/apps/$APP_ID.svg" "$SRC_ICON"
    same_as_source "$WORK_APPDIR/$APP_ID.desktop" "$SRC_DESKTOP"
    same_as_source "$WORK_APPDIR/$APP_ID.svg" "$SRC_ICON"
    local diricon
    diricon=$(file -b -- "$WORK_APPDIR/.DirIcon")
    [[ ! -L $WORK_APPDIR/.DirIcon && $diricon == "PNG image data, 256 x 256,"* ]] \
        || fail ".DirIcon is not a 256x256 PNG: $diricon"
    printf '    .DirIcon: %s (rendered from %s)\n' "$diricon" "${SRC_ICON#"$ROOT_DIR"/}"

    log "Checking the translations"
    local lang mo
    while IFS= read -r lang; do
        mo=$usr/share/locale/$lang/LC_MESSAGES/$GETTEXT_DOMAIN.mo
        [[ -s $mo ]] || fail "missing or empty: ${mo#"$WORK_APPDIR"/}"
        file -b -- "$mo" | grep -q 'GNU message catalog' \
            || fail "not a GNU message catalog: ${mo#"$WORK_APPDIR"/}"
        printf '    %s: %s\n' "$lang" "$(file -b -- "$mo")"
    done < <(linguas)

    log "Validating the desktop entry"
    desktop-file-validate -- "$usr/share/applications/$APP_ID.desktop"
    log "Validating the MetaInfo"
    appstreamcli validate --no-net --explain -- "$usr/share/metainfo/$APP_ID.metainfo.xml"
}

# ---- 4. Pinned tools ----

# fetch_tool PREFIX FILE VAR: the tool tools.lock pins as PREFIX_*, kept in
# out/tools/FILE, downloaded if missing and verified by SHA-256 on every use;
# its path is stored in the variable named VAR.
fetch_tool() {
    local prefix=$1 file=$2
    local -n path_var=$3
    local release url sha256 arch path
    release=$(lock_value "${prefix}_RELEASE")
    url=$(lock_value "${prefix}_URL")
    sha256=$(lock_value "${prefix}_SHA256")
    arch=$(lock_value "${prefix}_ARCH")
    [[ $arch == "$(uname -m)" ]] || fail "tools.lock: $prefix is for $arch, this machine is $(uname -m)"
    [[ $url == https://* ]] || fail "tools.lock: ${prefix}_URL must be https"

    path=$TOOLS_DIR/$file
    if [[ ! -f $path ]]; then
        log "Downloading $file"
        mkdir -p -- "$TOOLS_DIR"
        curl --proto '=https' --tlsv1.2 -fsSL -o "$path.download" -- "$url"
        mv -- "$path.download" "$path"
    fi
    log "Verifying $file"
    if ! printf '%s  %s\n' "$sha256" "$path" | sha256sum --check --status; then
        rm -f -- "$path"
        fail "$file does not match its SHA-256 in tools.lock (removed; it will be downloaded again)"
    fi
    printf '    %s  %s\n' "$sha256" "${path#"$ROOT_DIR"/}"
    chmod +x -- "$path"
    path_var=$path
}

LINUXDEPLOY=
APPIMAGETOOL=
RUNTIME=

fetch_linuxdeploy() {
    fetch_tool LINUXDEPLOY "linuxdeploy-$(lock_value LINUXDEPLOY_RELEASE)-$(lock_value LINUXDEPLOY_ARCH).AppImage" LINUXDEPLOY
}

# ---- 5. Libraries ----

deploy_libraries() {
    # The SVG loader is a module gdk-pixbuf opens at run time, so linuxdeploy
    # is told about it to deploy it and its libraries (librsvg) too.
    local -a args=(--appdir "$WORK_APPDIR" --library "$SVG_LOADER")
    local pattern
    while IFS= read -r pattern; do
        args+=(--exclude-library "$pattern")
    done < <(exclude_patterns)

    log "Deploying libraries with linuxdeploy (log: ${LINUXDEPLOY_LOG#"$ROOT_DIR"/})"
    # linuxdeploy is itself an AppImage; in a container without FUSE it
    # extracts itself and runs from the extracted files.
    # Its own copying of copyright files is turned off: it misses packages
    # that register their files under /lib rather than /usr/lib (liblzo2-2),
    # so add_license_documents does it for every package instead.
    if ! env -u LD_LIBRARY_PATH -u LD_PRELOAD APPIMAGE_EXTRACT_AND_RUN=1 DISABLE_COPYRIGHT_FILES_DEPLOYMENT=1 \
        "$LINUXDEPLOY" "${args[@]}" >"$LINUXDEPLOY_LOG" 2>&1; then
        tail -n 30 -- "$LINUXDEPLOY_LOG" >&2
        fail "linuxdeploy failed (full log: ${LINUXDEPLOY_LOG#"$ROOT_DIR"/})"
    fi
    grep -E '^(WARNING|ERROR)' -- "$LINUXDEPLOY_LOG" | sed 's/^/    linuxdeploy: /' || true
    printf '    left to the host by linuxdeploy'"'"'s own exclude list or exclude-libs.txt:\n'
    sed -n 's|^Skipping deployment of blacklisted library .*/\([^/ ]*\) *$|\1|p' -- "$LINUXDEPLOY_LOG" \
        | sort -u | paste -sd' ' - | fold -s -w 72 | sed 's/^/      /'

    # linuxdeploy links AppRun to the binary when there is none. The real
    # AppRun is installed by a later stage, so this placeholder is removed;
    # so are the empty icon and pixmap directories linuxdeploy prepares.
    if [[ -L $WORK_APPDIR/AppRun ]]; then
        rm -- "$WORK_APPDIR/AppRun"
        printf '    removed the AppRun link linuxdeploy created (replaced by ours later)\n'
    fi
    [[ ! -e $WORK_APPDIR/AppRun ]] || fail "unexpected AppRun in the AppDir"
    find "$WORK_APPDIR/usr/share" -mindepth 1 -type d -empty -delete
}

# ---- 6. GTK runtime resources ----

# The build environment's files add_gtk_resources copies, for finding their
# packages (add_license_documents).
RESOURCE_SOURCES=()

add_gtk_resources() {
    local usr=$WORK_APPDIR/usr

    log "Adding GTK's runtime resources"
    # GSettings schemas: GTK's own, only those listed, compiled for the
    # bundled GLib (found through GSETTINGS_SCHEMA_DIR, set by AppRun).
    local schema_dir=$usr/share/glib-2.0/schemas schema
    mkdir -p -- "$schema_dir"
    for schema in "${GTK_SCHEMAS[@]}"; do
        install -m644 -- "$SYSTEM_SCHEMA_DIR/$schema.gschema.xml" "$schema_dir/"
        RESOURCE_SOURCES+=("$SYSTEM_SCHEMA_DIR/$schema.gschema.xml")
    done
    glib-compile-schemas --strict -- "$schema_dir"
    printf '    schemas: %s\n' "${GTK_SCHEMAS[*]}"

    # gdk-pixbuf's loader cache, listing only the SVG loader linuxdeploy put
    # in usr/lib. The loader is named without a directory: gdk-pixbuf then
    # opens it like a library, through libgmodule's RUNPATH ($ORIGIN, that
    # is usr/lib), wherever the AppImage is mounted.
    local loader=${SVG_LOADER##*/}
    [[ -f $usr/lib/$loader ]] || fail "linuxdeploy did not deploy $loader"
    mkdir -p -- "$usr/lib/gdk-pixbuf-2.0/2.10.0"
    "$PIXBUF_QUERY_LOADERS" "$usr/lib/$loader" \
        | sed -e "s|^\"$usr/lib/|\"|" -e '/^# LoaderDir = /d' \
        > "$usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
    grep -qx "\"$loader\"" -- "$usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache" \
        || fail "unexpected gdk-pixbuf loader cache"
    printf '    gdk-pixbuf loaders: %s\n' "$loader"

    # No GIO modules: GIO_MODULE_DIR (AppRun) points here, so the bundled
    # GLib never loads the host's modules, built for another GLib.
    mkdir -p -- "$usr/lib/gio/modules"

    # Icons GTK and libadwaita lack, from Adwaita into hicolor, which every
    # icon theme falls back to; with hicolor's index.theme, so they are found
    # whatever the host provides.
    local hicolor=$usr/share/icons/hicolor name source context count=0
    while read -r name source context; do
        [[ $source == adwaita ]] || continue
        install -Dm644 -- "$ADWAITA_SYMBOLIC_DIR/$context/$name.svg" "$hicolor/scalable/$context/$name.svg"
        RESOURCE_SOURCES+=("$ADWAITA_SYMBOLIC_DIR/$context/$name.svg")
        count=$((count + 1))
    done < <(icon_entries)
    install -m644 -- "$HICOLOR_INDEX" "$hicolor/index.theme"
    RESOURCE_SOURCES+=("$HICOLOR_INDEX")
    printf '    icons: %d from Adwaita, with hicolor'"'"'s index.theme\n' "$count"
}

# ---- 7. AppRun ----

install_apprun() {
    log "Installing AppRun"
    # linuxdeploy's placeholder (a link to the binary) was removed already;
    # AppRun is the launcher from this directory, as a regular file.
    [[ ! -e $WORK_APPDIR/AppRun && ! -L $WORK_APPDIR/AppRun ]] || fail "unexpected AppRun before installing ours"
    install -m755 -- "$APPRUN_SRC" "$WORK_APPDIR/AppRun"
    same_as_source "$WORK_APPDIR/AppRun" "$APPRUN_SRC"
}

# ---- Licences ----

# The one package (without architecture) dpkg says owns a file named $1
# under the multiarch directory -- by name, so a package that registered it
# under /lib instead of /usr/lib (merged /usr) is found too.
package_of_library() {
    local name=$1 owners
    owners=$(dpkg -S -- "*/x86_64-linux-gnu/$name" "*/x86_64-linux-gnu/*/$name" 2>/dev/null \
        | awk -F': ' -v n="/$name" 'substr($2, length($2) - length(n) + 1) == n { print $1 }' \
        | tr ',' '\n' | sed 's/^ *//; s/:.*//' | sort -u || true)
    [[ -n $owners && $owners != *$'\n'* ]] || fail "no single Ubuntu package owns $name (found: ${owners:-none})"
    printf '%s' "$owners"
}

# Every bundled Ubuntu package's copyright file, exactly as the package
# installed it in the build environment (Debian's /usr/share/doc/<package>/
# copyright, the package's licence statement), and the app's own licence.
# check-appdir.sh fails the build if any bundled package lacks its file.
add_license_documents() {
    local usr=$WORK_APPDIR/usr
    log "Adding licence documents"
    local -A packages=()
    local path pkg
    for path in "$usr"/lib/*.so*; do
        [[ -f $path && ! -L $path ]] || continue
        pkg=$(package_of_library "${path##*/}")
        packages[$pkg]=1
    done
    for path in "${RESOURCE_SOURCES[@]}"; do
        pkg=$(dpkg -S -- "$path" 2>/dev/null | head -n 1 | cut -d: -f1)
        [[ -n $pkg ]] || fail "no Ubuntu package owns $path"
        packages[$pkg]=1
    done
    for pkg in "${!packages[@]}"; do
        [[ -s /usr/share/doc/$pkg/copyright ]] || fail "the package $pkg has no copyright file in the build environment"
        install -Dm644 -- "/usr/share/doc/$pkg/copyright" "$usr/share/doc/$pkg/copyright"
    done

    # Debian copyright files give most licences' full texts only by reference
    # to /usr/share/common-licenses/<name>, which hosts other than Debian's
    # do not have: the texts referred to are copied too (from base-files,
    # whose own copyright file comes with them), links such as GPL -> GPL-3
    # included.
    local name target count=0
    while IFS= read -r name; do
        path=/usr/share/common-licenses/$name
        [[ -e $path ]] || fail "a copyright file refers to $path, which the build environment does not have"
        mkdir -p -- "$usr/share/common-licenses"
        if [[ -L $path ]]; then
            target=$(readlink -- "$path")
            [[ $target != */* ]] || fail "unexpected link $path -> $target"
            ln -sfn -- "$target" "$usr/share/common-licenses/$name"
            path=/usr/share/common-licenses/$target
            name=$target
        fi
        install -m644 -- "$path" "$usr/share/common-licenses/$name"
        RESOURCE_SOURCES+=("$path")
        count=$((count + 1))
    done < <(grep -rhoE '/usr/share/common-licenses/[A-Za-z0-9.+_-]+' -- "$usr/share/doc" \
        | sed -e 's|^/usr/share/common-licenses/||' -e 's/[.]*$//' | sort -u)
    if [[ $count -gt 0 ]]; then
        pkg=$(dpkg -S -- /usr/share/common-licenses/GPL-2 2>/dev/null | head -n 1 | cut -d: -f1)
        [[ -n $pkg && -s /usr/share/doc/$pkg/copyright ]] || fail "cannot find the package of the common licence texts"
        install -Dm644 -- "/usr/share/doc/$pkg/copyright" "$usr/share/doc/$pkg/copyright"
        packages[$pkg]=1
    fi

    # The app's own licence, and the licences of the Rust crates compiled
    # into it (named apart from the Ubuntu packages' copyright files).
    install -Dm644 -- "$ROOT_DIR/LICENSE" "$usr/share/doc/$BIN_NAME/LICENSE"
    install -Dm644 -- "$SRC_RUST_DOCUMENT" "$usr/share/doc/$BIN_NAME/THIRD-PARTY-LICENSES.txt"
    same_as_source "$usr/share/doc/$BIN_NAME/THIRD-PARTY-LICENSES.txt" "$SRC_RUST_DOCUMENT"
    printf '    copyright files of %d packages, %d common licence texts they refer to, the app'"'"'s LICENSE\n' \
        "${#packages[@]}" "$(find "$usr/share/common-licenses" -mindepth 1 2>/dev/null | wc -l)"
    printf '    and THIRD-PARTY-LICENSES.txt (%s Rust crates)\n' \
        "$(sed -n 's/^distributed-crates\t//p' -- "$RUST_LICENSE_MANIFEST.partial")"
}

# ---- 8. The bundle ----

check_bundle() {
    log "Checking the bundle (check-appdir.sh)"
    env GLIBC_MAX="$GLIBC_MAX" "$SCRIPT_DIR/check-appdir.sh" "$WORK_APPDIR" "$MANIFEST.partial" "$LICENSE_MANIFEST.partial" \
        "$RUST_LICENSE_MANIFEST.partial"
}

# ---- 9. The AppDir is complete ----

finish() {
    mv -- "$WORK_APPDIR" "$APPDIR"
    mv -- "$MANIFEST.partial" "$MANIFEST"
    mv -- "$LICENSE_MANIFEST.partial" "$LICENSE_MANIFEST"
    mv -- "$RUST_DOCUMENT.partial" "$RUST_DOCUMENT"
    mv -- "$RUST_LICENSE_MANIFEST.partial" "$RUST_LICENSE_MANIFEST"
    log "AppDir ready: ${APPDIR#"$ROOT_DIR"/}"
    printf '    bundle manifest: %s\n' "${MANIFEST#"$ROOT_DIR"/}"
    printf '    licence manifest: %s\n' "${LICENSE_MANIFEST#"$ROOT_DIR"/}"
    printf '    Rust licence manifest: %s\n' "${RUST_LICENSE_MANIFEST#"$ROOT_DIR"/}"
    printf '    Rust crates'"'"' licence document: %s\n' "${RUST_DOCUMENT#"$ROOT_DIR"/}"
    printf '\nContents (type, mode, size, path; usr/lib and usr/share/doc summarized):\n'
    (cd -- "$APPDIR" && find . -mindepth 1 \
        -not -path './usr/lib/*' -not -path './usr/share/doc/*' \
        -printf '    %y %M %9s  %p\n' | sort -k4)
    printf '    usr/lib: %d files, %s\n' "$(find "$APPDIR/usr/lib" -type f | wc -l)" \
        "$(du -sh -- "$APPDIR/usr/lib" | cut -f1)"
    printf '    usr/share/doc: %d files (copyright files of the bundled packages; the app'"'"'s licences)\n' \
        "$(find "$APPDIR/usr/share/doc" -type f | wc -l)"
    printf '    total: %s\n' "$(du -sh -- "$APPDIR" | cut -f1)"
    printf '\nSymbolic links:\n'
    (cd -- "$APPDIR" && find . -type l -printf '    %p -> %l\n' | sort)
}

# ---- 10. The AppImage ----

APPIMAGE_NAME=

# The timestamp every file in the AppImage gets: SOURCE_DATE_EPOCH if given
# (e.g. the release commit's time), else the Ubuntu snapshot's -- an input of
# the build either way, so the same inputs give the same AppImage.
source_date_epoch() {
    if [[ -n ${SOURCE_DATE_EPOCH:-} ]]; then
        [[ $SOURCE_DATE_EPOCH =~ ^[0-9]+$ ]] || fail "SOURCE_DATE_EPOCH must be a number of seconds"
        printf '%s' "$SOURCE_DATE_EPOCH"
        return
    fi
    local snapshot
    snapshot=$(apt-config dump | sed -n 's/^APT::Snapshot "\([0-9]\{8\}T[0-9]\{6\}Z\)";$/\1/p')
    [[ -n $snapshot ]] || fail "no Ubuntu snapshot configured, and no SOURCE_DATE_EPOCH given"
    date -u -d "${snapshot:0:4}-${snapshot:4:2}-${snapshot:6:2}T${snapshot:9:2}:${snapshot:11:2}:${snapshot:13:2}Z" +%s
}

build_appimage() {
    fetch_tool APPIMAGETOOL "appimagetool-$(lock_value APPIMAGETOOL_RELEASE)-$(lock_value APPIMAGETOOL_ARCH).AppImage" APPIMAGETOOL
    fetch_tool RUNTIME "runtime-$(lock_value RUNTIME_RELEASE)-$(lock_value RUNTIME_ARCH)" RUNTIME

    local version label epoch
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' -- "$ROOT_DIR/Cargo.toml" | head -n 1)
    label=${APPIMAGE_VERSION:-$version-test}
    [[ $label =~ ^[0-9A-Za-z.+_-]+$ ]] || fail "unusable version label: $label"
    # A name without "-test" is a release's: only build-aux/release/release.sh,
    # after checking the tag, the commit and the version, may produce one.
    if [[ $label != *-test && ${LIW_OFFICIAL_RELEASE:-} != 1 ]]; then
        fail "APPIMAGE_VERSION=$label names a release; official artifacts are built only by build-aux/release/release.sh (use a label ending in -test for a test build)"
    fi
    APPIMAGE_NAME=LinuxImageWriter-$label-x86_64.AppImage
    epoch=$(source_date_epoch)

    log "Building $APPIMAGE_NAME (file times: $(date -u -d "@$epoch" +%Y-%m-%dT%H:%M:%SZ))"
    mkdir -p -- "$DIST_DIR"
    # --no-appstream: appimagetool only looks for the old name
    # (<id>.appdata.xml); the MetaInfo (<id>.metainfo.xml) was validated with
    # appstreamcli above. The squashfs gets fixed times and root ownership,
    # so it does not depend on when or by whom it was built. The times are
    # given as mksquashfs options, so SOURCE_DATE_EPOCH must not reach it
    # (mksquashfs refuses both at once).
    if ! env -u LD_LIBRARY_PATH -u LD_PRELOAD -u SOURCE_DATE_EPOCH APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 \
        "$APPIMAGETOOL" \
        --no-appstream \
        --runtime-file "$RUNTIME" \
        --comp zstd \
        --mksquashfs-opt -all-root \
        --mksquashfs-opt -all-time --mksquashfs-opt "$epoch" \
        --mksquashfs-opt -mkfs-time --mksquashfs-opt "$epoch" \
        "$APPDIR" "$DIST_DIR/$APPIMAGE_NAME.partial" >"$OUT_DIR/appimagetool.log" 2>&1; then
        tail -n 30 -- "$OUT_DIR/appimagetool.log" >&2
        fail "appimagetool failed (full log: ${OUT_DIR#"$ROOT_DIR"/}/appimagetool.log)"
    fi
    grep -E '^WARNING' -- "$OUT_DIR/appimagetool.log" | sed 's/^/    appimagetool: /' || true
}

# ---- 11. Checks on the AppImage ----

check_appimage() {
    local image=$DIST_DIR/$APPIMAGE_NAME.partial

    log "Checking the AppImage"
    [[ -x $image ]] || fail "the AppImage is not executable"
    local type
    type=$(file -b -- "$image")
    [[ $type == "ELF 64-bit LSB"*"x86-64"* ]] || fail "unexpected AppImage file type: $type"
    # Type 2 AppImage magic: "AI" and 2 at offset 8 of the ELF header.
    [[ $(od -An -tx1 -j8 -N3 -- "$image" | tr -d ' \n') == 414902 ]] || fail "not a type 2 AppImage (magic)"
    printf '    type 2 AppImage; %s\n' "${type%%, BuildID*}"

    # The runtime at its start is the pinned one: the same size (the
    # squashfs starts where it ends), and the same bytes except the sections
    # appimagetool fills in (update information, signature, digest).
    local offset runtime_size
    offset=$(env -u LD_LIBRARY_PATH "$image" --appimage-offset)
    runtime_size=$(stat -c %s -- "$RUNTIME")
    [[ $offset == "$runtime_size" ]] || fail "the squashfs starts at $offset, not after the $runtime_size-byte runtime"
    local ranges="" name start size
    while read -r name start size; do
        ranges+="$((16#$start)) $((16#$start + 16#$size))"$'\n'
    done < <(readelf -S --wide -- "$RUNTIME" \
        | sed -n 's/^ *\[ *[0-9]*\] \(\.\(upd_info\|sha256_sig\|sig_key\|digest_md5\)\) *[A-Z]* *[0-9a-f]* \([0-9a-f]*\) \([0-9a-f]*\).*/\1 \3 \4/p')
    [[ $(grep -c . <<<"$ranges") -ge 3 ]] || fail "could not read the runtime's data sections"
    local changed
    # (cmp exits 1 when the bytes differ, which they do in those sections.)
    changed=$({ cmp -l -- <(head -c "$offset" -- "$image") "$RUNTIME" 2>/dev/null || true; } | awk -v ranges="$ranges" '
        BEGIN { n = split(ranges, r, "\n"); for (i = 1; i <= n; i++) { split(r[i], p, " "); lo[i] = p[1] + 0; hi[i] = p[2] + 0 } }
        { pos = $1 - 1; ok = 0; for (i = 1; i <= n; i++) if (pos >= lo[i] && pos < hi[i]) ok = 1; if (!ok) bad++ }
        END { print bad + 0 }')
    [[ $changed == 0 ]] || fail "the runtime in the AppImage differs from the pinned one outside its data sections ($changed bytes)"
    printf '    runtime: the pinned %s (%s bytes), unchanged but for its data sections\n' \
        "$(lock_value RUNTIME_RELEASE)" "$runtime_size"

    # Its contents are exactly the AppDir, and pass the same checks.
    rm -rf -- "$EXTRACT_DIR"
    mkdir -p -- "$EXTRACT_DIR"
    (cd -- "$EXTRACT_DIR" && env -u LD_LIBRARY_PATH "$image" --appimage-extract >/dev/null)
    local extracted=$EXTRACT_DIR/squashfs-root
    diff -r --no-dereference -- "$APPDIR" "$extracted" >/dev/null \
        || fail "the AppImage's contents differ from the AppDir"
    diff <(cd -- "$APPDIR" && find . -printf '%p %y %m %l\n' | sort) \
         <(cd -- "$extracted" && find . -printf '%p %y %m %l\n' | sort) >/dev/null \
        || fail "the AppImage's file types, modes or links differ from the AppDir"
    printf '    contents: identical to the AppDir (files, modes, links)\n'
    env GLIBC_MAX="$GLIBC_MAX" "$SCRIPT_DIR/check-appdir.sh" "$extracted" "$EXTRACT_DIR/bundle-manifest.txt" "$EXTRACT_DIR/license-manifest.txt" \
        "$RUST_LICENSE_MANIFEST" | sed 's/^/    /'
    diff -- "$MANIFEST" "$EXTRACT_DIR/bundle-manifest.txt" >/dev/null \
        || fail "the extracted AppImage's bundle manifest differs from the AppDir's"
    diff -- "$LICENSE_MANIFEST" "$EXTRACT_DIR/license-manifest.txt" >/dev/null \
        || fail "the extracted AppImage's licence manifest differs from the AppDir's"
    printf '    bundle and licence manifests of the extracted AppImage: identical to the AppDir'"'"'s\n'
    local doc
    for doc in "$SRC_RUST_DOCUMENT" "$RUST_DOCUMENT"; do
        cmp -s -- "$extracted/usr/share/doc/$BIN_NAME/THIRD-PARTY-LICENSES.txt" "$doc" \
            || fail "the extracted AppImage's THIRD-PARTY-LICENSES.txt differs from ${doc#"$ROOT_DIR"/}"
    done
    printf '    THIRD-PARTY-LICENSES.txt of the extracted AppImage: identical to %s and to the generated one\n' \
        "${SRC_RUST_DOCUMENT#"$ROOT_DIR"/}"
    rm -rf -- "$EXTRACT_DIR"
}

finish_appimage() {
    mv -- "$DIST_DIR/$APPIMAGE_NAME.partial" "$DIST_DIR/$APPIMAGE_NAME"
    (cd -- "$DIST_DIR" && sha256sum -- "$APPIMAGE_NAME" > "$APPIMAGE_NAME.sha256")
    local appdir_bytes image_bytes
    appdir_bytes=$(du -sb --apparent-size -- "$APPDIR" | cut -f1)
    image_bytes=$(stat -c %s -- "$DIST_DIR/$APPIMAGE_NAME")
    log "AppImage ready: ${DIST_DIR#"$ROOT_DIR"/}/$APPIMAGE_NAME"
    printf '    %s\n' "$(cat -- "$DIST_DIR/$APPIMAGE_NAME.sha256")"
    printf '    %d bytes (AppDir: %d bytes; %d%%)\n' "$image_bytes" "$appdir_bytes" $((image_bytes * 100 / appdir_bytes))
    [[ $APPIMAGE_NAME == *-test-* ]] \
        && printf '    a TEST build: not for publishing (set APPIMAGE_VERSION for a release)\n'
    return 0
}

# ---- Main ----

main() {
    check_environment
    clean_output
    check_icon_list
    build_binary
    generate_rust_licenses
    assemble_appdir
    check_binary
    check_app_files
    fetch_linuxdeploy
    deploy_libraries
    add_gtk_resources
    install_apprun
    add_license_documents
    check_app_files
    check_bundle
    finish
    build_appimage
    check_appimage
    finish_appimage
}

main "$@"
