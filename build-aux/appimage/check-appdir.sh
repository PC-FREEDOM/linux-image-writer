#!/usr/bin/env bash
# Checks every ELF file in an AppDir and writes its bundle manifest.
#
#   check-appdir.sh APPDIR MANIFEST [LICENSE_MANIFEST [RUST_LICENSE_MANIFEST]]
#
# Run inside the AppImage build container (build.sh calls it): package
# ownership is looked up in its dpkg database, and libraries the AppDir does
# not contain are resolved against its Ubuntu 24.04 system.
#
# Fails (exit 1), naming each file at fault, if the AppDir contains:
#   - a library that exclude-libs.txt says is never bundled
#   - a file from a package that is never bundled (glibc, Mesa and GPU
#     drivers, UDisks2, D-Bus, polkit, the kernel)
#   - a GPU driver module, a kernel module, or a daemon
#   - an executable other than the app's, or one with an unexpected
#     interpreter
#   - an ELF file whose dependencies do not resolve, which needs a glibc
#     symbol version newer than GLIBC_MAX (2.39) or GLIBC_PRIVATE, or whose
#     RPATH / RUNPATH points outside the AppDir
#   - a dependency resolved outside the AppDir that host-libs.txt does not
#     allow
#   - an AppRun that is missing, a link, or sets the library path, preloads,
#     the display backend, renderer, theme, GTK module path (GTK_PATH), GTK
#     data prefix or D-Bus addresses, or calls a privilege tool
#   - an AppRun that does not set and export GTK_EXE_PREFIX, exactly once,
#     to exactly "$APPDIR/usr" before running the app (so the bundled GTK
#     loads no host GTK modules), or an AppDir with GTK modules of its own
#     (usr/lib/gtk-4.0)
#   - GSettings schemas other than GTK's own, or no compiled schemas
#   - any GIO module, or a gdk-pixbuf loader other than the SVG loader, or a
#     loader cache naming a loader by path or one not in usr/lib
#   - an icon icons.txt does not account for, or one it lists that is missing
#   - a .DirIcon that is not a 256x256 PNG
#   - a bundled package whose licences license-review.txt does not classify
#     (a licence name it lacks, or a copyright file not reviewed or changed
#     since its review)
#   - a bundled Ubuntu package without its copyright file in
#     usr/share/doc/<package>/, or with one that differs from the package's;
#     a licence document of no bundled package; or no copy of the app's own
#     LICENSE
#   - no THIRD-PARTY-LICENSES.txt (the licences of the Rust crates compiled
#     into the app), one that differs from the repository's
#     data/THIRD-PARTY-LICENSES.txt, or, given RUST_LICENSE_MANIFEST (written
#     by build-aux/rust-licenses/generate.sh), one whose SHA-256 is not the
#     one that manifest records
#
# The manifest lists every file with its SHA-256, and for each ELF file its
# SONAME, Ubuntu package and version, newest glibc symbol version, RUNPATH,
# NEEDED, and where each NEEDED library resolves; then the libraries left to
# the host. It contains nothing time- or path-dependent, so two builds from
# the same inputs produce the same manifest, and `diff` between two releases'
# manifests shows exactly what changed in the bundle.

set -euo pipefail
shopt -s nullglob
export LC_ALL=C

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly SCRIPT_DIR
readonly EXCLUDE_LIST=$SCRIPT_DIR/exclude-libs.txt
readonly HOST_LIBS=$SCRIPT_DIR/host-libs.txt
readonly ICONS_LIST=$SCRIPT_DIR/icons.txt
readonly APP_ID=io.github.pc_freedom.linux-image-writer
readonly TOOLS_LOCK=$SCRIPT_DIR/tools.lock
readonly GLIBC_MAX=${GLIBC_MAX:-2.39}
readonly EXPECTED_INTERP=/lib64/ld-linux-x86-64.so.2
readonly MULTIARCH_DIR=/usr/lib/x86_64-linux-gnu

# The only executables the AppDir may contain (paths relative to it).
readonly ALLOWED_EXECUTABLES=(usr/bin/linux-image-writer)

# Ubuntu packages whose files are never bundled.
readonly FORBIDDEN_PACKAGES='^(libc6|libc6-[a-z0-9-]+|libc-bin|udisks2|libudisks2-0|dbus|dbus-bin|dbus-daemon|dbus-broker|dbus-system-bus-common|dbus-session-bus-common|polkitd|pkexec|policykit-1|linux-(image|modules)(-[a-z0-9.+-]+)?|libgl1|libgl1-mesa-dri|libglx0|libglx-mesa0|libegl1|libegl-mesa0|libopengl0|libgles1|libgles2|libglvnd0|libglapi-mesa|libgbm1|libdrm2|libdrm-[a-z0-9]+|mesa-[a-z0-9-]+|libnvidia-[a-z0-9-]+|nvidia-[a-z0-9-]+)$'

# Paths (relative to the AppDir) that are never bundled, whatever their
# package: GPU driver modules and their ICD / vendor files, kernel modules.
readonly FORBIDDEN_PATHS=(
    '*/dri/*' '*_dri.so' '*_drv_video.so' '*/gbm/*'
    '*/vulkan/icd.d/*' '*/glvnd/*' '*/egl/egl_external_platform.d/*'
    '*.ko' '*.ko.*' '*/lib/modules/*'
)

# Programs that are never bundled, by file name.
readonly FORBIDDEN_PROGRAMS=(udisksd dbus-daemon dbus-broker dbus-broker-launch polkitd polkit-agent-helper-1 pkexec sudo)

usage() { printf 'usage: %s APPDIR MANIFEST [LICENSE_MANIFEST [RUST_LICENSE_MANIFEST]]\n' "${0##*/}" >&2; exit 2; }
[[ $# -ge 2 && $# -le 4 ]] || usage
[[ -d $1 ]] || { printf 'check-appdir.sh: not a directory: %s\n' "$1" >&2; exit 2; }
APPDIR=$(cd -- "$1" && pwd -P)
readonly APPDIR MANIFEST=$2 LICENSE_MANIFEST=${3:-} RUST_LICENSE_MANIFEST=${4:-}
readonly REPO_LICENSE=$SCRIPT_DIR/../../LICENSE
readonly LICENSE_REVIEW=$SCRIPT_DIR/license-review.txt
readonly RUST_DOC=usr/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt
readonly REPO_RUST_DOC=$SCRIPT_DIR/../../data/THIRD-PARTY-LICENSES.txt

failures=0
fail_file() {
    printf 'check-appdir.sh: FAIL  %s: %s\n' "$1" "$2" >&2
    failures=$((failures + 1))
}

# The patterns in exclude-libs.txt (first word of each non-comment line).
exclude_patterns=()
while read -r pattern _; do
    exclude_patterns+=("$pattern")
done < <(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$EXCLUDE_LIST")
readonly exclude_patterns

matches_exclude() {
    local name=$1 pattern
    for pattern in "${exclude_patterns[@]}"; do
        # shellcheck disable=SC2053  # the pattern is a glob on purpose
        [[ $name == $pattern ]] && { printf '%s' "$pattern"; return 0; }
    done
    return 1
}

is_elf() { [[ $(head -c 4 -- "$1" | od -An -tx1 | tr -d ' \n') == 7f454c46 ]]; }

# The highest of a list of version numbers (one per line).
max_version() { sort -V | tail -n 1; }

version_gt() { [[ $1 != "$2" && $(printf '%s\n%s\n' "$1" "$2" | max_version) == "$1" ]]; }

# ---- Inventory ----

files=() elfs=() links=()
while IFS= read -r -d '' path; do
    rel=${path#"$APPDIR"/}
    if [[ -L $path ]]; then
        links+=("$rel")
    elif [[ -f $path ]]; then
        files+=("$rel")
        is_elf "$path" && elfs+=("$rel")
    fi
done < <(find "$APPDIR" -mindepth 1 \( -type f -o -type l \) -print0 | sort -z)

# Ubuntu package of each bundled library, looked up by file name in the
# multiarch directory (the AppDir copies are renamed to their SONAME and
# patched, so their contents cannot be matched).
declare -A pkg_of=() origin_of=()
lookup_names=()
for rel in "${elfs[@]}"; do
    name=${rel##*/}
    if [[ -e $MULTIARCH_DIR/$name ]]; then
        origin_of[$name]=$(readlink -f -- "$MULTIARCH_DIR/$name")
        lookup_names+=("*/x86_64-linux-gnu/$name" "*/x86_64-linux-gnu/${origin_of[$name]##*/}")
    else
        # A module from a subdirectory (a gdk-pixbuf loader): the single
        # file of that name under the multiarch directory.
        found=$(find "$MULTIARCH_DIR" -name "$name" -type f 2>/dev/null | sort | head -n 2)
        if [[ -n $found && $found != *$'\n'* ]]; then
            origin_of[$name]=$found
            lookup_names+=("$found")
        fi
    fi
done
if [[ ${#lookup_names[@]} -gt 0 ]]; then
    while IFS= read -r line; do
        pkg=${line%%: *}
        file=${line#*: }
        name=${file##*/}
        pkg_of[$name]=${pkg_of[$name]:-$pkg}
        pkg_of[$file]=${pkg_of[$file]:-$pkg}
    done < <(dpkg -S "${lookup_names[@]}" 2>/dev/null || true)
fi

package_of() {
    local name=$1 pkg=${pkg_of[$1]:-}
    [[ -z $pkg && -n ${origin_of[$name]:-} ]] && pkg=${pkg_of[${origin_of[$name]##*/}]:-}
    [[ -z $pkg && -n ${origin_of[$name]:-} ]] && pkg=${pkg_of[${origin_of[$name]}]:-}
    if [[ -n $pkg ]]; then
        printf '%s\t%s' "${pkg%%,*}" "$(dpkg-query -W -f='${Version}' -- "${pkg%%,*}")"
    else
        printf -- '-\t-'
    fi
}

# ---- Checks per file ----

for rel in "${files[@]}" "${links[@]}"; do
    for pattern in "${FORBIDDEN_PATHS[@]}"; do
        # shellcheck disable=SC2053
        [[ /$rel == $pattern ]] && fail_file "$rel" "path matches never-bundled pattern '$pattern'"
    done
    for program in "${FORBIDDEN_PROGRAMS[@]}"; do
        [[ ${rel##*/} == "$program" ]] && fail_file "$rel" "never-bundled program '$program'"
    done
done

declare -A host_dependents=() host_resolved=() host_maxver=() package_files=()
elf_rows=() glibc_rows=()
newest_glibc=0 newest_glibc_files=()
executables=0 libraries=0

for rel in "${elfs[@]}"; do
    path=$APPDIR/$rel
    name=${rel##*/}
    type=$(readelf -h -- "$path" | awk '/^ *Type:/ { print $2 }')
    interp=$(readelf -l -- "$path" | sed -n 's/.*Requesting program interpreter: \(.*\)\]$/\1/p')
    soname=$(readelf -d -- "$path" | sed -n 's/.*(SONAME).*\[\(.*\)\]$/\1/p')
    mapfile -t needed < <(readelf -d -- "$path" | sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p')
    runpath=$(readelf -d -- "$path" | sed -n 's/.*(RUNPATH).*\[\(.*\)\]$/\1/p')
    rpath=$(readelf -d -- "$path" | sed -n 's/.*(RPATH).*\[\(.*\)\]$/\1/p')

    # Executables: only the app, with the system's dynamic loader. A library
    # can carry an interpreter too, to be runnable (libcap prints its
    # version): one with a SONAME is counted as a library, but its
    # interpreter is still checked.
    if [[ -n $interp && -n $soname ]]; then
        [[ $interp == "$EXPECTED_INTERP" ]] || fail_file "$rel" "unexpected interpreter '$interp' (expected $EXPECTED_INTERP)"
        type="$type+interp"
        libraries=$((libraries + 1))
    elif [[ -n $interp || $type == EXEC ]]; then
        executables=$((executables + 1))
        allowed=0
        for exe in "${ALLOWED_EXECUTABLES[@]}"; do [[ $rel == "$exe" ]] && allowed=1; done
        [[ $allowed -eq 1 ]] || fail_file "$rel" "unexpected executable (interpreter: ${interp:-none})"
        [[ $interp == "$EXPECTED_INTERP" ]] || fail_file "$rel" "unexpected interpreter '${interp:-none}' (expected $EXPECTED_INTERP)"
    else
        libraries=$((libraries + 1))
    fi

    # Never-bundled libraries, by file name and SONAME.
    candidates=("$name")
    [[ -n $soname && $soname != "$name" ]] && candidates+=("$soname")
    for candidate in "${candidates[@]}"; do
        if pattern=$(matches_exclude "$candidate"); then
            fail_file "$rel" "never-bundled library '$candidate' (exclude-libs.txt: $pattern)"
        fi
    done

    # Never-bundled packages.
    pkgver=$(package_of "$name")
    pkg=${pkgver%%$'\t'*}
    pkg=${pkg%%:*}
    if [[ $pkg =~ $FORBIDDEN_PACKAGES ]]; then
        fail_file "$rel" "from never-bundled package '$pkg'"
    fi
    if [[ ${#ALLOWED_EXECUTABLES[@]} -gt 0 && " ${ALLOWED_EXECUTABLES[*]} " == *" $rel "* ]]; then
        pkgver=$'(built from this repository)\t-'
    elif [[ $pkg == - || -z $pkg ]]; then
        fail_file "$rel" "no Ubuntu package found for it (its licence cannot be traced)"
    else
        package_files[$pkg]+="$rel "
    fi

    # Library search paths must stay inside the AppDir.
    for entry in ${runpath//:/ } ${rpath//:/ }; do
        # shellcheck disable=SC2016  # a literal $ORIGIN
        [[ $entry == '$ORIGIN' || $entry == '$ORIGIN/'* ]] \
            || fail_file "$rel" "RUNPATH/RPATH entry '$entry' is not relative to \$ORIGIN"
    done

    # glibc symbol versions this file requires.
    mapfile -t versions < <(objdump -p -- "$path" | awk '/required from/ { req = 1; next } req && /^ +0x[0-9a-f]+ 0x[0-9a-f]+ [0-9]+ / { print $NF }' | sort -u)
    glibc=0
    for v in "${versions[@]}"; do
        case $v in
            GLIBC_PRIVATE) fail_file "$rel" "requires GLIBC_PRIVATE" ;;
            GLIBC_[0-9]*)
                version=${v#GLIBC_}
                version_gt "$version" "$glibc" && glibc=$version
                ;;
        esac
    done
    if [[ $glibc != 0 ]]; then
        symbols=$(objdump -T -- "$path" | awk -v v="(GLIBC_$glibc)" '
            /\*UND\*/ && index($0, v) { printf "%s%s(%s)", sep, $NF, ($0 ~ /^[0-9a-f]+ +w /) ? "weak" : "strong"; sep = "," }')
        glibc_rows+=("$rel"$'\t'"GLIBC_$glibc"$'\t'"${symbols:--}")
        if version_gt "$glibc" "$GLIBC_MAX"; then
            fail_file "$rel" "requires GLIBC_$glibc (limit $GLIBC_MAX): ${symbols:-no symbol listed}"
        fi
        if version_gt "$glibc" "$newest_glibc"; then
            newest_glibc=$glibc
            newest_glibc_files=("$rel")
        elif [[ $glibc == "$newest_glibc" ]]; then
            newest_glibc_files+=("$rel")
        fi
    fi

    # Where each dependency resolves: inside the AppDir, or on the host.
    declare -A resolved=()
    while IFS= read -r line; do
        if [[ $line =~ ^[[:space:]]*([^[:space:]]+)\ =\>\ not\ found ]]; then
            resolved[${BASH_REMATCH[1]}]='NOT-FOUND'
        elif [[ $line =~ ^[[:space:]]*([^[:space:]]+)\ =\>\ (/[^[:space:]]+)\ \( ]]; then
            resolved[${BASH_REMATCH[1]}]=$(readlink -f -- "${BASH_REMATCH[2]}")
        fi
    done < <(env -u LD_LIBRARY_PATH -u LD_PRELOAD ldd -- "$path" 2>&1 || true)
    needed_desc=()
    for lib in "${needed[@]}"; do
        target=${resolved[$lib]:-}
        if [[ $lib == ld-linux-x86-64.so.2 ]]; then
            needed_desc+=("$lib=host")
            host_dependents[$lib]+="$rel "
            host_resolved[$lib]=$(readlink -f -- "$EXPECTED_INTERP")
        elif [[ -z $target || $target == NOT-FOUND ]]; then
            fail_file "$rel" "unresolved dependency '$lib'"
            needed_desc+=("$lib=NOT-FOUND")
        elif [[ $target == "$APPDIR"/* ]]; then
            needed_desc+=("$lib=appdir")
        else
            needed_desc+=("$lib=host")
            host_dependents[$lib]+="$rel "
            host_resolved[$lib]=$target
        fi
    done
    # Any unresolved indirect dependency is reported too.
    for lib in "${!resolved[@]}"; do
        [[ ${resolved[$lib]} == NOT-FOUND ]] && ! printf '%s\n' "${needed[@]}" | grep -qxF -- "$lib" \
            && fail_file "$rel" "unresolved indirect dependency '$lib'"
    done
    unset resolved

    # The newest version this file requires from each host library.
    while read -r lib ver; do
        [[ -n ${host_resolved[$lib]:-} || $lib == ld-linux-x86-64.so.2 ]] || continue
        if [[ -z ${host_maxver[$lib]:-} ]] || [[ $(printf '%s\n%s\n' "${host_maxver[$lib]}" "$ver" | max_version) == "$ver" ]]; then
            host_maxver[$lib]=$ver
        fi
    done < <(objdump -p -- "$path" | awk '/required from/ { lib = $3; sub(/:$/, "", lib); next } lib != "" && /^ +0x[0-9a-f]+ 0x[0-9a-f]+ [0-9]+ / { print lib, $NF }')

    origin=${origin_of[$name]:--}
    elf_rows+=("$rel"$'\t'"$type"$'\t'"${soname:--}"$'\t'"$pkgver"$'\t'"${origin}"$'\t'"$( [[ $glibc != 0 ]] && printf 'GLIBC_%s' "$glibc" || printf -- '-')"$'\t'"${runpath:--}${rpath:+ (RPATH $rpath)}"$'\t'"$(IFS=,; printf '%s' "${needed_desc[*]:--}")")
done

# ---- Libraries left to the host ----

allowed_host=()
while read -r lib _; do
    allowed_host+=("$lib")
done < <(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$HOST_LIBS")
is_allowed_host() {
    local lib
    for lib in "${allowed_host[@]}"; do [[ $1 == "$lib" ]] && return 0; done
    return 1
}
for lib in "${!host_resolved[@]}"; do
    if ! is_allowed_host "$lib"; then
        fail_file "$(tr ' ' '\n' <<<"${host_dependents[$lib]}" | sed '/^$/d' | sort -u | paste -sd, -)" \
            "needs '$lib' from outside the AppDir, and host-libs.txt does not allow it"
    fi
done
unused_host=()
for lib in "${allowed_host[@]}"; do
    [[ -n ${host_resolved[$lib]:-} ]] || unused_host+=("$lib")
done

# ---- Runtime resources ----

# AppRun: a regular script that only points at the AppDir's resources.
apprun=$APPDIR/AppRun
if [[ ! -e $apprun && ! -L $apprun ]]; then
    fail_file AppRun "missing"
elif [[ -L $apprun ]]; then
    fail_file AppRun "is a symbolic link, not the launcher script"
else
    [[ -x $apprun ]] || fail_file AppRun "not executable"
    [[ $(head -n 1 -- "$apprun") == '#!/bin/sh' ]] || fail_file AppRun "does not start with #!/bin/sh"
    code=$(sed -e 's/#.*//' -- "$apprun")
    for var in LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT GDK_BACKEND GSK_RENDERER GTK_THEME GTK_PATH GTK_MODULES \
               GTK_DATA_PREFIX DBUS_SYSTEM_BUS_ADDRESS DBUS_SESSION_BUS_ADDRESS; do
        grep -qE "(^|[^A-Za-z0-9_])$var=" <<<"$code" && fail_file AppRun "sets $var"
    done
    # GTK_EXE_PREFIX is required: GTK looks for its loadable modules
    # (immodules, media, printbackends) in $GTK_EXE_PREFIX/lib/gtk-4.0, and
    # otherwise in the directory it was built for, which on Debian and Ubuntu
    # hosts holds the host's GTK modules, built for another GTK and GLib. It
    # must point into the AppImage, at "$APPDIR/usr" exactly.
    mapfile -t prefix_lines < <(grep -nE '(^|[^A-Za-z0-9_])GTK_EXE_PREFIX=' <<<"$code")
    exec_line=$(grep -nE '^exec ' <<<"$code" | head -n 1 | cut -d: -f1)
    if [[ ${#prefix_lines[@]} -eq 0 ]]; then
        fail_file AppRun "does not set GTK_EXE_PREFIX (required: GTK_EXE_PREFIX=\"\$APPDIR/usr\", so the bundled GTK loads no host GTK modules)"
    elif [[ ${#prefix_lines[@]} -gt 1 ]]; then
        fail_file AppRun "sets GTK_EXE_PREFIX more than once (lines $(printf '%s\n' "${prefix_lines[@]}" | cut -d: -f1 | paste -sd, -)); set it once, to \"\$APPDIR/usr\""
    else
        line=${prefix_lines[0]#*:}
        line=$(sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' <<<"$line")
        if [[ $line != 'GTK_EXE_PREFIX="$APPDIR/usr"' ]]; then
            fail_file AppRun "sets GTK_EXE_PREFIX as '$line', not GTK_EXE_PREFIX=\"\$APPDIR/usr\" (it must point inside the AppImage, never at a host or other path)"
        fi
        if [[ -n $exec_line && ${prefix_lines[0]%%:*} -gt $exec_line ]]; then
            fail_file AppRun "sets GTK_EXE_PREFIX only after running the app"
        fi
        grep -qE '^[[:space:]]*export([[:space:]].*)?[[:space:]]GTK_EXE_PREFIX([[:space:]]|$)' <<<"$code" \
            || fail_file AppRun "does not export GTK_EXE_PREFIX (the app would not see it)"
    fi
    grep -qwE 'sudo|pkexec|doas|su|runuser|setpriv|capsh' <<<"$code" && fail_file AppRun "calls a privilege tool"
    grep -qE '^exec "\$APPDIR/usr/bin/linux-image-writer" "\$@"$' <<<"$code" \
        || fail_file AppRun "does not end by executing usr/bin/linux-image-writer"
fi
# The modules directory GTK_EXE_PREFIX leads GTK to: the AppImage has none.
for rel in "${files[@]}" "${links[@]}"; do
    [[ $rel == usr/lib/gtk-4.0/* ]] && fail_file "$rel" "GTK module (the AppImage loads none: AppRun points GTK_EXE_PREFIX at usr, which has no lib/gtk-4.0)"
done

# GSettings: only GTK's own schemas, compiled.
schema_dir=usr/share/glib-2.0/schemas
schemas=()
if [[ -d $APPDIR/$schema_dir ]]; then
    for rel in "${files[@]}"; do
        [[ $rel == "$schema_dir"/* ]] || continue
        case ${rel##*/} in
            gschemas.compiled) ;;
            org.gtk.gtk4.*.gschema.xml) schemas+=("${rel##*/}") ;;
            *) fail_file "$rel" "not one of GTK's own GSettings schemas" ;;
        esac
    done
    [[ -s $APPDIR/$schema_dir/gschemas.compiled ]] || fail_file "$schema_dir" "no gschemas.compiled"
else
    fail_file "$schema_dir" "missing"
fi

# GIO modules: none, only the empty directory AppRun points GIO at.
if [[ -d $APPDIR/usr/lib/gio/modules ]]; then
    for rel in "${files[@]}" "${links[@]}"; do
        [[ $rel == usr/lib/gio/modules/* ]] && fail_file "$rel" "GIO module (the AppImage loads none)"
    done
else
    fail_file usr/lib/gio/modules "missing (AppRun points GIO_MODULE_DIR at it)"
fi
for rel in "${files[@]}"; do
    [[ $rel == */gio/modules/* && $rel != usr/lib/gio/modules/* ]] && fail_file "$rel" "GIO module"
done

# gdk-pixbuf: the SVG loader only, named in the cache without a directory.
readonly ALLOWED_PIXBUF_LOADERS=(libpixbufloader-svg.so)
cache=usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache
cached_loaders=()
if [[ -f $APPDIR/$cache ]]; then
    while IFS= read -r loader; do
        cached_loaders+=("$loader")
        [[ $loader == */* ]] && fail_file "$cache" "names loader '$loader' by path"
        [[ -f $APPDIR/usr/lib/${loader##*/} ]] || fail_file "$cache" "names '$loader', which is not in usr/lib"
    done < <(sed -n 's/^"\([^"]*\.so\)"$/\1/p' -- "$APPDIR/$cache")
else
    fail_file "$cache" "missing"
fi
for rel in "${files[@]}"; do
    name=${rel##*/}
    [[ $name == libpixbufloader-*.so ]] || continue
    allowed=0
    for loader in "${ALLOWED_PIXBUF_LOADERS[@]}"; do [[ $name == "$loader" ]] && allowed=1; done
    [[ $allowed -eq 1 ]] || fail_file "$rel" "gdk-pixbuf loader not needed by the app"
    printf '%s\n' "${cached_loaders[@]}" | grep -qxF -- "$name" || fail_file "$rel" "loader not in $cache"
done

# Icons: the app's own, the ones icons.txt copies from Adwaita, and
# hicolor's index.theme -- nothing else; and every icon icons.txt says GTK
# or libadwaita embeds must really be embedded.
declare -A expected_icons=(
    ["usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"]=app
    ["usr/share/icons/hicolor/index.theme"]=index
)
embedded=$( { strings -- "$APPDIR/usr/lib/libgtk-4.so.1"; strings -- "$APPDIR/usr/lib/libadwaita-1.so.0"; } 2>/dev/null \
    | grep -oE '[a-z0-9-]+-symbolic\.(svg|symbolic\.png)' | sed -E 's/\.(svg|symbolic\.png)$//' | sort -u)
while read -r name source context; do
    case $source in
        adwaita)
            expected_icons["usr/share/icons/hicolor/scalable/$context/$name.svg"]=adwaita
            [[ -f $APPDIR/usr/share/icons/hicolor/scalable/$context/$name.svg ]] \
                || fail_file "usr/share/icons/hicolor/scalable/$context/$name.svg" "missing (icons.txt)"
            ;;
        gtk)
            grep -qxF -- "$name" <<<"$embedded" || fail_file "$name" "icons.txt says GTK or libadwaita embeds it, but they do not"
            ;;
    esac
done < <(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$ICONS_LIST")
for rel in "${files[@]}" "${links[@]}"; do
    [[ $rel == usr/share/icons/* ]] || continue
    [[ -n ${expected_icons[$rel]:-} ]] || fail_file "$rel" "icon not accounted for by icons.txt"
done

# .DirIcon: a 256x256 PNG, as the AppImage specification asks.
if [[ -L $APPDIR/.DirIcon || ! -f $APPDIR/.DirIcon ]]; then
    fail_file .DirIcon "missing, or not a regular file"
else
    diricon=$(file -b -- "$APPDIR/.DirIcon")
    [[ $diricon == "PNG image data, 256 x 256,"* ]] || fail_file .DirIcon "not a 256x256 PNG ($diricon)"
fi

# Where each non-ELF file comes from, for the manifest.
resource_origin() {
    local rel=$1 src pkg
    case $rel in
        AppRun|usr/share/applications/*|usr/share/metainfo/*|usr/share/locale/*|"usr/share/icons/hicolor/scalable/apps/$APP_ID.svg")
            printf '(this repository)'; return ;;
        */gschemas.compiled) printf '(generated by glib-compile-schemas)'; return ;;
        .DirIcon) printf '(rendered from the app icon by rsvg-convert)'; return ;;
        */loaders.cache) printf '(generated by gdk-pixbuf-query-loaders)'; return ;;
        usr/share/glib-2.0/schemas/*) src=/usr/share/glib-2.0/schemas/${rel##*/} ;;
        usr/share/common-licenses/*) src=/usr/share/common-licenses/${rel##*/} ;;
        usr/share/icons/hicolor/index.theme) src=/usr/share/icons/hicolor/index.theme ;;
        usr/share/icons/hicolor/scalable/*)
            src=${rel#usr/share/icons/hicolor/scalable/}
            src=/usr/share/icons/Adwaita/symbolic/$src ;;
        "usr/share/doc/linux-image-writer/LICENSE") printf '(this repository)'; return ;;
        "usr/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt")
            printf '(this repository: data/THIRD-PARTY-LICENSES.txt, generated by build-aux/rust-licenses)'; return ;;
        usr/share/doc/*/copyright)
            src=${rel#usr/share/doc/}; src=${src%%/*}
            printf '%s %s' "$src" "$(dpkg-query -W -f='${Version}' -- "$src" 2>/dev/null || printf '?')"; return ;;
        *) printf -- '-'; return ;;
    esac
    pkg=$(dpkg -S -- "$src" 2>/dev/null | head -n 1 | cut -d: -f1)
    if [[ -n $pkg ]]; then
        printf '%s %s (%s)' "$pkg" "$(dpkg-query -W -f='${Version}' -- "$pkg" 2>/dev/null || printf '?')" "$src"
    else
        printf -- '- (%s)' "$src"
    fi
}

# ---- Licences ----

# The packages of the non-ELF files copied from Ubuntu packages.
for rel in "${files[@]}"; do
    is_elf "$APPDIR/$rel" && continue
    [[ $rel == usr/share/doc/* ]] && continue
    origin=$(resource_origin "$rel")
    case $origin in
        '('*|'-'*) ;;
        *) package_files[${origin%% *}]+="$rel " ;;
    esac
done

# Every bundled package has its copyright file, identical to the one the
# package installed in the build environment.
for pkg in "${!package_files[@]}"; do
    doc=usr/share/doc/$pkg/copyright
    if [[ ! -s $APPDIR/$doc || -L $APPDIR/$doc ]]; then
        fail_file "$doc" "missing: the copyright file of the bundled package $pkg"
    elif [[ ! -s /usr/share/doc/$pkg/copyright ]]; then
        fail_file "$doc" "the build environment has no copyright file for $pkg to compare with"
    elif ! cmp -s -- "$APPDIR/$doc" "/usr/share/doc/$pkg/copyright"; then
        fail_file "$doc" "differs from the copyright file of the package $pkg"
    fi
done
# No licence document of a package that is not bundled, and nothing else
# under usr/share/doc but those and the app's own LICENSE.
for rel in "${files[@]}" "${links[@]}"; do
    [[ $rel == usr/share/doc/* ]] || continue
    if [[ $rel == usr/share/doc/linux-image-writer/LICENSE ]]; then
        cmp -s -- "$APPDIR/$rel" "$REPO_LICENSE" || fail_file "$rel" "differs from the repository's LICENSE"
        continue
    fi
    [[ $rel == "$RUST_DOC" ]] && continue
    pkg=${rel#usr/share/doc/}; pkg=${pkg%%/*}
    [[ $rel == "usr/share/doc/$pkg/copyright" ]] || fail_file "$rel" "unexpected file in usr/share/doc"
    [[ -n ${package_files[$pkg]:-} ]] || fail_file "$rel" "licence document of a package that is not bundled"
done
[[ -f $APPDIR/usr/share/doc/linux-image-writer/LICENSE ]] \
    || fail_file usr/share/doc/linux-image-writer/LICENSE "missing: the app's own licence"

# The licences of the Rust crates compiled into the app: the document
# build-aux/rust-licenses/generate.sh wrote and checked, unchanged.
rust_doc_sha=-
if [[ -L $APPDIR/$RUST_DOC || ! -s $APPDIR/$RUST_DOC ]]; then
    fail_file "$RUST_DOC" "missing: the licences of the Rust crates compiled into the app"
else
    rust_doc_sha=$(sha256sum -- "$APPDIR/$RUST_DOC" | cut -d' ' -f1)
    cmp -s -- "$APPDIR/$RUST_DOC" "$REPO_RUST_DOC" || fail_file "$RUST_DOC" "differs from the repository's data/THIRD-PARTY-LICENSES.txt"
    [[ $(head -n 1 -- "$APPDIR/$RUST_DOC") == "Linux Image Writer: licences of the Rust crates it is built from" ]] \
        || fail_file "$RUST_DOC" "not the Rust crates' licence document (unexpected first line)"
fi
rust_value() { sed -n "s/^$1\t//p" -- "$RUST_LICENSE_MANIFEST" | head -n 1; }
if [[ -n $RUST_LICENSE_MANIFEST ]]; then
    if [[ ! -s $RUST_LICENSE_MANIFEST ]]; then
        fail_file "$RUST_LICENSE_MANIFEST" "missing or empty"
    elif [[ $(rust_value document-sha256) != "$rust_doc_sha" ]]; then
        fail_file "$RUST_DOC" "SHA-256 $rust_doc_sha is not the one the Rust licence manifest records ($(rust_value document-sha256))"
    fi
fi

# Every licence text a copyright file refers to (/usr/share/common-licenses/
# <name>) is in the AppDir, identical to the build environment's; no other.
declare -A referenced_texts=()
while IFS=$'\t' read -r name count; do
    referenced_texts[$name]=$count
done < <(grep -rhoE '/usr/share/common-licenses/[A-Za-z0-9.+_-]+' -- "$APPDIR/usr/share/doc" 2>/dev/null \
    | sed -e 's|^/usr/share/common-licenses/||' -e 's/[.]*$//' | sort | uniq -c | awk '{ print $2 "\t" $1 }')
for name in "${!referenced_texts[@]}"; do
    text=usr/share/common-licenses/$name
    if [[ ! -e $APPDIR/$text ]]; then
        fail_file "$text" "missing: a copyright file refers to /usr/share/common-licenses/$name"
    elif [[ $(readlink -f -- "$APPDIR/$text") != "$APPDIR"/usr/share/common-licenses/* ]]; then
        fail_file "$text" "resolves outside usr/share/common-licenses"
    elif ! cmp -s -- "$APPDIR/$text" "/usr/share/common-licenses/$name"; then
        fail_file "$text" "differs from the build environment's /usr/share/common-licenses/$name"
    fi
done
for rel in "${files[@]}" "${links[@]}"; do
    [[ $rel == usr/share/common-licenses/* ]] || continue
    name=${rel##*/}
    if [[ -z ${referenced_texts[$name]:-} ]]; then
        # A text only a link refers to (GPL -> GPL-3) counts as referenced.
        linked=0
        for link in "${links[@]}"; do
            [[ $link == usr/share/common-licenses/* && $(readlink -- "$APPDIR/$link") == "$name" \
               && -n ${referenced_texts[${link##*/}]:-} ]] && linked=1
        done
        [[ $linked -eq 1 ]] || fail_file "$rel" "licence text no copyright file refers to"
    fi
done

# ---- Licence classes ----

# license-review.txt: [names] (licence name -> class) and [packages]
# (non-machine-readable copyright files, by SHA-256 -> classes).
declare -A name_class=() review_sha=() review_classes=()
section=
while read -r first second third _; do
    case $first in
        '['*']') section=$first; continue ;;
    esac
    case $section in
        '[names]') name_class[$first]=$second ;;
        '[packages]') review_sha[$first]=$second; review_classes[$first]=$third ;;
    esac
done < <(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -- "$LICENSE_REVIEW")

# The single licence names in a DEP-5 file's "License:" fields: split at
# "or", "and" and ",", without "with ... exception".
licence_atoms() {
    sed -n 's/^License: *\(.*[^ ]\) *$/\1/p' -- "$1" \
        | sed -E 's/, and /\n/g; s/ and /\n/g; s/ or /\n/g; s/ OR /\n/g' \
        | sed -E 's/^ *//; s/,$//; s/ with .*//' | sed '/^$/d' | sort -u
}

declare -A pkg_classes=() pkg_source=()
for pkg in "${!package_files[@]}"; do
    doc=usr/share/doc/$pkg/copyright
    [[ -s $APPDIR/$doc ]] || continue
    classes=
    if grep -q '^Format:' -- "$APPDIR/$doc"; then
        while IFS= read -r atom; do
            class=${name_class[$atom]:-}
            if [[ -z $class ]]; then
                fail_file "$doc" "licence name '$atom' is not classified in license-review.txt"
                class=unreviewed
            fi
            classes+="$class"$'\n'
        done < <(licence_atoms "$APPDIR/$doc")
    else
        sha=$(sha256sum -- "$APPDIR/$doc" | cut -d' ' -f1)
        if [[ -z ${review_sha[$pkg]:-} ]]; then
            fail_file "$doc" "not machine-readable, and license-review.txt has no review of it"
            classes=unreviewed
        elif [[ ${review_sha[$pkg]} != "$sha" ]]; then
            fail_file "$doc" "changed since its review in license-review.txt (SHA-256 $sha)"
            classes=unreviewed
        else
            classes=${review_classes[$pkg]//,/$'\n'}
        fi
    fi
    pkg_classes[$pkg]=$(sort -u <<<"$classes" | sed '/^$/d' | paste -sd, -)
    pkg_source[$pkg]=no
    grep -qvxE 'permissive|public-domain' < <(tr ',' '\n' <<<"${pkg_classes[$pkg]}") && pkg_source[$pkg]=yes
done

# The licence names a machine-readable (DEP-5) copyright file declares.
licence_names() {
    if grep -q '^Format:' -- "$1"; then
        sed -n 's/^License: *\(.*[^ ]\) *$/\1/p' -- "$1" | sort -u | paste -sd';' - | sed 's/;/; /g'
    else
        printf '(not machine-readable; see the document)'
    fi
}

if [[ -n $LICENSE_MANIFEST ]]; then
    {
        printf '# Linux Image Writer AppImage licence manifest (format 1)\n'
        printf '# Written by build-aux/appimage/check-appdir.sh. Tab-separated; one row per\n'
        printf '# bundled Ubuntu package (and the app), with the licence document the\n'
        printf '# AppImage carries for it. Paths are relative to the AppDir.\n'
        printf '\n[inputs]\n'
        # shellcheck source=/dev/null
        printf 'os\t%s\n' "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")"
        printf 'ubuntu-snapshot\t%s\n' "$(apt-config dump | sed -n 's/^APT::Snapshot "\(.*\)";$/\1/p')"
        printf '\n[summary]\n'
        present=0
        for pkg in "${!package_files[@]}"; do
            [[ -s $APPDIR/usr/share/doc/$pkg/copyright ]] && present=$((present + 1))
        done
        printf 'packages\t%d\n' "${#package_files[@]}"
        printf 'with-document\t%d\n' "$present"
        printf 'without-document\t%d\n' $((${#package_files[@]} - present))
        source_count=0
        for pkg in "${!pkg_source[@]}"; do [[ ${pkg_source[$pkg]} == yes ]] && source_count=$((source_count + 1)); done
        printf 'source-required\t%d\n' "$source_count"
        printf 'source-not-required\t%d\n' $((${#package_files[@]} - source_count))
        printf '\n[packages]\n'
        printf '# package\tversion\tsource-package\tsource-version\tclasses\tsource-required\tlicences (DEP-5)\tdocument\tdocument-sha256\tdocument-origin\tbundled-files\n'
        for pkg in "${!package_files[@]}"; do
            doc=usr/share/doc/$pkg/copyright
            printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$pkg" \
                "$(dpkg-query -W -f='${Version}\t${source:Package}\t${source:Version}' -- "$pkg" 2>/dev/null || printf -- '-\t-\t-')" \
                "${pkg_classes[$pkg]:--}" "${pkg_source[$pkg]:-yes}" \
                "$( [[ -s $APPDIR/$doc ]] && licence_names "$APPDIR/$doc" || printf -- '-')" \
                "$( [[ -s $APPDIR/$doc ]] && printf '%s' "$doc" || printf 'MISSING')" \
                "$( [[ -s $APPDIR/$doc ]] && sha256sum -- "$APPDIR/$doc" | cut -d' ' -f1 || printf -- '-')" \
                "Ubuntu package (/usr/share/doc/$pkg/copyright)" \
                "$(tr ' ' '\n' <<<"${package_files[$pkg]}" | sed '/^$/d' | sort -u | paste -sd, -)"
        done | sort
        printf '\n[common-licenses]\n'
        printf '# The full licence texts the copyright files refer to as /usr/share/common-licenses/<name>.\n'
        printf '# name\tdocument\tdocument-sha256\treferences\tdocument-origin\n'
        for name in "${!referenced_texts[@]}"; do
            text=usr/share/common-licenses/$name
            printf '%s\t%s\t%s\t%s\t%s\n' "$name" \
                "$text$( [[ -L $APPDIR/$text ]] && printf ' -> %s' "$(readlink -- "$APPDIR/$text")")" \
                "$( [[ -e $APPDIR/$text ]] && sha256sum -- "$APPDIR/$text" | cut -d' ' -f1 || printf 'MISSING')" \
                "${referenced_texts[$name]}" \
                "$(p=$(dpkg -S -- "/usr/share/common-licenses/$name" 2>/dev/null | head -n 1 | cut -d: -f1); printf '%s %s' "${p:--}" "$(dpkg-query -W -f='${Version}' -- "$p" 2>/dev/null)")"
        done | sort
        printf '\n[app]\n'
        printf '# package\tversion\tlicence\tdocument\tdocument-sha256\tdocument-origin\n'
        app_doc=usr/share/doc/linux-image-writer/LICENSE
        printf 'linux-image-writer\t%s\t%s\t%s\t%s\t%s\n' \
            "$(sed -n 's/^version = "\(.*\)"$/\1/p' -- "$SCRIPT_DIR/../../Cargo.toml" | head -n 1)" \
            "$(sed -n 's/^license = "\(.*\)"$/\1/p' -- "$SCRIPT_DIR/../../Cargo.toml" | head -n 1)" \
            "$app_doc" \
            "$( [[ -f $APPDIR/$app_doc ]] && sha256sum -- "$APPDIR/$app_doc" | cut -d' ' -f1 || printf -- '-')" \
            "this repository (LICENSE)"
        printf 'linux-image-writer (Rust crates)\t%s\t%s\t%s\t%s\t%s\n' \
            "$( [[ -n $RUST_LICENSE_MANIFEST ]] && printf '%s crates' "$(rust_value distributed-crates)" || printf -- '-')" \
            "$( [[ -n $RUST_LICENSE_MANIFEST ]] && sed -n 's/^used-under\t\(.*\)\t\([0-9]*\)$/\1 (\2)/p' -- "$RUST_LICENSE_MANIFEST" | paste -sd';' - | sed 's/;/; /g' || printf -- '-')" \
            "$RUST_DOC" "$rust_doc_sha" \
            "this repository (data/THIRD-PARTY-LICENSES.txt, generated from Cargo.lock by build-aux/rust-licenses, cargo-about $( [[ -n $RUST_LICENSE_MANIFEST ]] && rust_value cargo-about | cut -f1 || printf '?'))"
        if [[ -n $RUST_LICENSE_MANIFEST ]]; then
            printf '\n[rust-crates]\n'
            printf '# The Rust crates compiled into the app, each listed with its licence in\n'
            printf '# %s; rust-license-manifest.txt has the details.\n' "$RUST_DOC"
            printf 'rust-license-manifest-sha256\t%s\n' "$(sha256sum -- "$RUST_LICENSE_MANIFEST" | cut -d' ' -f1)"
            for key in distributed-crates linked proc-macro excluded clarified licence-texts; do
                printf '%s\t%s\n' "$key" "$(rust_value "$key")"
            done
        fi
    } > "$LICENSE_MANIFEST"
fi

# ---- Manifest ----

lock_value() { sed -n "s/^$1=//p" -- "$TOOLS_LOCK" | head -n 1; }

{
    printf '# Linux Image Writer AppDir bundle manifest (format 1)\n'
    printf '# Written by build-aux/appimage/check-appdir.sh. Tab-separated; compare\n'
    printf '# two builds with diff. Paths are relative to the AppDir.\n'
    printf '\n[inputs]\n'
    # shellcheck source=/dev/null
    printf 'os\t%s\n' "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")"
    printf 'ubuntu-snapshot\t%s\n' "$(apt-config dump | sed -n 's/^APT::Snapshot "\(.*\)";$/\1/p')"
    printf 'linuxdeploy\t%s %s\n' "$(lock_value LINUXDEPLOY_RELEASE)" "$(lock_value LINUXDEPLOY_SHA256)"

    printf '\n[summary]\n'
    printf 'files\t%d\n' "${#files[@]}"
    printf 'symlinks\t%d\n' "${#links[@]}"
    printf 'elf-files\t%d\n' "${#elfs[@]}"
    printf 'executables\t%d\n' "$executables"
    printf 'shared-libraries\t%d\n' "$libraries"
    printf 'host-libraries\t%d\n' "${#host_resolved[@]}"
    printf 'glibc-newest\tGLIBC_%s\t%s\n' "$newest_glibc" "$(IFS=,; printf '%s' "${newest_glibc_files[*]}")"
    printf 'glibc-limit\tGLIBC_%s\n' "$GLIBC_MAX"

    printf '\n[elf]\n'
    printf '# path\ttype\tsoname\tpackage\tversion\tbuild-env-origin\tglibc-newest\trunpath\tneeded(=where it resolves)\n'
    printf '%s\n' "${elf_rows[@]}" | sort

    printf '\n[host-libraries]\n'
    printf '# Not bundled: the host must provide these.\n'
    printf '# soname\tnewest-version-needed\tbuild-env-file\tpackage\tversion\tneeded-by\n'
    for lib in "${!host_resolved[@]}"; do
        file=${host_resolved[$lib]}
        printf '%s\t%s\t%s\t%s\t%s\n' "$lib" "${host_maxver[$lib]:--}" "$file" \
            "$(p=$(dpkg -S -- "$file" 2>/dev/null | head -n1 | cut -d: -f1); [[ -n $p ]] && printf '%s\t%s' "$p" "$(dpkg-query -W -f='${Version}' -- "$p:amd64" 2>/dev/null || dpkg-query -W -f='${Version}' -- "$p")" || printf -- '-\t-')" \
            "$(tr ' ' '\n' <<<"${host_dependents[$lib]}" | sed '/^$/d' | sort -u | paste -sd, -)"
    done | sort

    printf '\n[host-allowlist-unused]\n'
    printf '# Entries of host-libs.txt no ELF file needs at present.\n'
    [[ ${#unused_host[@]} -eq 0 ]] || printf '%s\n' "${unused_host[@]}" | sort

    printf '\n[resources]\n'
    printf '# Files that are not ELF, and where they come from.\n'
    printf '# path\torigin\n'
    for rel in "${files[@]}"; do
        is_elf "$APPDIR/$rel" && continue
        printf '%s\t%s\n' "$rel" "$(resource_origin "$rel")"
    done

    printf '\n[glibc]\n'
    printf '# path\tnewest-glibc-version\tsymbols-at-that-version\n'
    printf '%s\n' "${glibc_rows[@]}" | sort

    printf '\n[symlinks]\n'
    for rel in "${links[@]}"; do
        printf '%s\t-> %s\n' "$rel" "$(readlink -- "$APPDIR/$rel")"
    done

    printf '\n[files]\n'
    printf '# sha256\tsize\tmode\tpath\n'
    for rel in "${files[@]}"; do
        printf '%s\t%s\t%s\t%s\n' "$(sha256sum -- "$APPDIR/$rel" | cut -d' ' -f1)" \
            "$(stat -c %s -- "$APPDIR/$rel")" "$(stat -c %a -- "$APPDIR/$rel")" "$rel"
    done
} > "$MANIFEST"

printf 'check-appdir.sh: %d ELF files (%d executable, %d libraries), %d host libraries, newest glibc GLIBC_%s (limit %s)\n' \
    "${#elfs[@]}" "$executables" "$libraries" "${#host_resolved[@]}" "$newest_glibc" "$GLIBC_MAX"
printf 'check-appdir.sh: %d bundled Ubuntu packages, each with its copyright file checked\n' "${#package_files[@]}"
printf 'check-appdir.sh: manifest written to %s\n' "$MANIFEST"
[[ -z $LICENSE_MANIFEST ]] || printf 'check-appdir.sh: licence manifest written to %s\n' "$LICENSE_MANIFEST"
if [[ $failures -gt 0 ]]; then
    printf 'check-appdir.sh: %d problem(s) found (listed above)\n' "$failures" >&2
    exit 1
fi
printf 'check-appdir.sh: PASS\n'
