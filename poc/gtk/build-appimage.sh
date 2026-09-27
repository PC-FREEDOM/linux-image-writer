#!/bin/sh
# Phase 3B-0A PoC: package the GTK PoC as an AppImage with linuxdeploy and
# its GTK plugin. The tools are not installed; pass the directory holding
# them (downloaded from the projects' GitHub releases):
#
#   linuxdeploy-x86_64.AppImage   github.com/linuxdeploy/linuxdeploy (continuous)
#   linuxdeploy-plugin-gtk.sh     github.com/linuxdeploy/linuxdeploy-plugin-gtk
#   appimagetool-x86_64.AppImage  github.com/AppImage/appimagetool (continuous)
#
#   ./build-appimage.sh /path/to/tools
#
# The AppDir and the AppImage go to ./appimage (ignored by git). The desktop
# file and icon are placeholders generated here; no branding.
set -eu

tools=$(realpath "$1")
here=$(dirname "$(realpath "$0")")
out="$here/appimage"
appdir="$out/AppDir"
app_id=io.github.pcfreedom.LinuxUsbWriter.GtkPoc

cd "$here"
cargo build --release --locked

rm -rf "$out"
mkdir -p "$out"
cat > "$out/$app_id.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Linux USB Writer GTK PoC
Exec=gtk-poc
Icon=$app_id
Categories=Utility;
Terminal=false
EOF
cat > "$out/$app_id.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256"><rect width="256" height="256" rx="32" fill="#3584e4"/></svg>
EOF

export PATH="$tools:$PATH"
export DEPLOY_GTK_VERSION=4
export ARCH=x86_64
export LDAI_OUTPUT="$out/gtk-poc-x86_64.AppImage"
"$tools/linuxdeploy-x86_64.AppImage" \
    --appdir "$appdir" \
    --executable target/release/gtk-poc \
    --desktop-file "$out/$app_id.desktop" \
    --icon-file "$out/$app_id.svg" \
    --plugin gtk \
    --output appimage
