<img src="data/icons/hicolor/scalable/apps/io.github.pc_freedom.linux-image-writer.svg" width="96" alt="">

# Linux Image Writer

[日本語](README.ja.md)

A simple USB image writer for Linux, designed with safety in mind.

*Simple by default, powerful when needed.*

## Status

Linux Image Writer is currently being developed toward its first release (v0.1).
No official packages are published yet.

## Features

- Writes raw disk images, such as ISO and IMG files
- Writes gzip and xz compressed images, decompressing them while writing
- Recognizes the image format from the file's contents, not only from its name
- Offers only USB drives that can be written to; system drives, protected
  drives and drives that are in use cannot be selected
- Checks the selected drive again right before writing
- Asks for a final confirmation that shows the drive to be written
- Optional verification after writing: Quick, Full or None
- Shows progress through preparing, writing and verifying
- Lets you cancel the operation
- Summarizes the outcome on a result screen
- Optional safe removal of the USB drive from the result screen
- GTK 4 and libadwaita user interface

### Not included

- Linux only
- No partition editor, filesystem formatter or persistence creator
- No image downloads
- No automatic unmounting before writing: a mounted drive cannot be selected
  until it is unmounted
- Compressed formats other than gzip and xz (for example zip, 7z, bzip2,
  zstd or lz4) are not supported

## Requirements

- Linux with UDisks2
- A polkit authentication agent (normally provided by the desktop environment)

The GUI is available in English and Japanese.

## Installation

No official packages are published yet.
A production Flatpak manifest is available for development builds; see
[Building from source](#building-from-source) for building the app itself.

## Usage

1. Choose the image to write.
2. Choose the target USB drive. If exactly one suitable drive is connected,
   it is selected automatically.
3. Choose a verification mode.
4. Press the write button.
5. In the final confirmation, check the target drive and confirm. Your system
   may ask you to authenticate.
6. Wait while the image is written and, if selected, verified. You can cancel
   the operation.
7. Review the result.
8. Optionally, safely remove the USB drive from the result screen.

**Writing erases all existing data on the target drive.**

## Verification

- **Quick** reads back part of the written data, balancing speed and coverage.
- **Full** reads back all of the written data. It takes longer.
- **None** does not read back the written data.

Quick verification is not available for gzip and xz compressed images; use
Full or None for them.

## Safe removal

After writing, the result screen offers to prepare a supported USB drive for
removal. This never happens automatically: you start it yourself. Linux Image
Writer unmounts the drive's filesystems if needed and then makes the drive
ready to be unplugged. Only when this succeeds does it tell you that the drive
can be safely removed.

If the drive is not supported, or is still in use, remove it from your file
manager or your desktop environment instead.

## Important notes

- Writing an image erases all existing data on the target drive.
- System drives and other protected or unsuitable drives are not offered as
  targets.
- The selected drive is checked again before anything is written.
- The final confirmation shows the target drive; check that it is the drive
  you intend to overwrite.
- Verification is optional.

Always make sure you selected the intended drive before confirming the write.

## Building from source

Requirements:

- Linux
- Rust 1.98.1 (pinned in `rust-toolchain.toml`)
- GTK 4.12 or later and libadwaita 1.5 or later, with their development files
- pkg-config
- A C compiler
- At runtime: UDisks2 and a polkit authentication agent

Build the GUI:

```sh
cargo build --release --features gui --bin linux-image-writer
```

The program is built as `target/release/linux-image-writer`.

The repository also contains `linux-image-writer-dev`, a command-line development
and diagnostic tool. It includes destructive test modes and is not intended
for general use.

## Reporting issues

Please report bugs and suggestions on
[GitHub Issues](https://github.com/PC-FREEDOM/linux-image-writer/issues).

## License

Linux Image Writer is licensed under the GNU General Public License v3.0 or
later (GPL-3.0-or-later). See [LICENSE](LICENSE).
