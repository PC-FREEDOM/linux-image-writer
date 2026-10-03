# Linux Image Writer AppImage

[日本語](AppImage.ja.md)

Starting with v0.1.1, each GitHub Release of Linux Image Writer includes an
AppImage: a single file that runs on most current Linux distributions
without installation.

## Download and verify

From the release page, download these two files (for example, for v0.1.1):

- `LinuxImageWriter-0.1.1-x86_64.AppImage`
- `LinuxImageWriter-0.1.1-x86_64.AppImage.sha256`

Check the download in the folder you saved it to:

```sh
sha256sum -c LinuxImageWriter-0.1.1-x86_64.AppImage.sha256
```

It must print `LinuxImageWriter-0.1.1-x86_64.AppImage: OK`. If it does not,
delete the file and download it again.

## Run

```sh
chmod +x LinuxImageWriter-0.1.1-x86_64.AppImage
./LinuxImageWriter-0.1.1-x86_64.AppImage
```

You can also make the file executable in your file manager (Properties →
Permissions) and open it from there.

The app never runs as root and asks for no password at start. Writing to a
USB drive goes through UDisks2, which may ask for authorization through your
desktop's polkit agent.

### If the AppImage does not start from a USB drive or other removable media

On removable media such as FAT-formatted USB drives, the desktop's mount
settings can prevent running a program directly from the drive (for
example, the file cannot be made executable). Copy the AppImage to your home
directory, `~/Downloads`, or another local folder, and run it from there.

### If FUSE is not available

The AppImage mounts itself with FUSE. If that fails (for example, in a
container without `/dev/fuse`), run it with:

```sh
./LinuxImageWriter-0.1.1-x86_64.AppImage --appimage-extract-and-run
```

This unpacks it to a temporary folder first (about 95 MB, which may be left
behind in `/tmp` after the app exits).

## Requirements

- x86_64 Linux with glibc 2.39 or later (Ubuntu 24.04, Debian 13, Fedora 40
  and later, openSUSE Tumbleweed, and similar)
- FUSE 3 (`fusermount3`, usually from the `fuse3` package), or
  `--appimage-extract-and-run` as above
- UDisks2, the D-Bus system and session buses, and a polkit authentication
  agent (normally provided by the desktop environment)
- `shared-mime-info` (installed on every desktop system): without it, the
  app's icons cannot be drawn
- Fonts for the language the app is shown in -- for Japanese, a font with
  CJK coverage (for example, Noto Sans CJK). The AppImage uses your system's
  fonts.
- Recommended: `xdg-desktop-portal`, so that the app uses your desktop's
  file dialog

The AppImage works on Wayland and X11 and with GNOME, KDE Plasma, Xfce and
other desktops. It bundles GTK 4 and libadwaita; your system's graphics
drivers (Mesa and others) are used as they are.

## Licences and source code

- Linux Image Writer is licensed under the GNU General Public License,
  version 3 or later.
- The licences of everything the AppImage contains are inside it, in
  `usr/share/doc/`: the app's `LICENSE`, the licences of the Rust crates
  compiled into it (`linux-image-writer/THIRD-PARTY-LICENSES.txt`), and the
  copyright file of each bundled Ubuntu library package. To read them,
  extract the AppImage with `--appimage-extract`.
- Two source archives are attached to each release:
  - `LinuxImageWriter-<version>-source.tar.gz`: the source code of Linux
    Image Writer itself, exactly as tagged for that release.
  - `LinuxImageWriter-<version>-appimage-sources.tar.zst`: the
    corresponding source code of the bundled libraries whose licences (GPL,
    LGPL, MPL and others) ask for it, exactly as Ubuntu published it.
    Most users do not need it.
