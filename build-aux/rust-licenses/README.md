# Rust crates' licences

The `linux-image-writer` GUI is built with about a hundred Rust crates, which
Rust links into the binary statically. Their licences (mostly MIT, offered
alongside Apache-2.0) ask that their copyright notices and licence texts go
with every copy of the program. This directory generates that document and
checks it; the result is kept in the repository as
**`data/THIRD-PARTY-LICENSES.txt`**, a distributed document like the desktop
entry and the MetaInfo beside it.

```
Cargo.lock, about.toml, the template, the crates' licence files, cargo-about
        │  build-aux/rust-licenses/generate.sh   (generate / --check)
        ▼
data/THIRD-PARTY-LICENSES.txt   (committed; never edited by hand)
        │
        ├─▶ AppImage: usr/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt
        └─▶ Flatpak:  /app/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt
```

It is not specific to one packaging: the crates are the same in the AppImage
and the Flatpak (the same `Cargo.lock`, the same `--features gui` binary, and
the same graph on x86_64 and aarch64), so both ship the same file. Neither
packaging generates it: the AppImage build regenerates it only to check that
the committed copy is current (and stops if not), and the Flatpak, an offline
build, only copies it.

## Files

| File | Purpose |
|---|---|
| `cargo-about.lock` | The pinned cargo-about release: version, commit, URL, SHA-256 of the archive and of the executable |
| `about.toml` | cargo-about's configuration: accepted licences, targets, and the crates that need clarifying |
| `THIRD-PARTY-LICENSES.txt.hbs` | The document's template (plain text) |
| `generate.sh` | Generates the document and the manifest, and runs the checks |
| `check.py` | The checks, and the manifest (`rust-license-manifest.txt`) |
| `../../data/THIRD-PARTY-LICENSES.txt` | The generated document, committed: what the AppImage and the Flatpak ship |

## Running it

In the AppImage build container, which has the pinned cargo-about, cargo and
Python. From the repository root:

```sh
run() {
  podman run --rm --security-opt label=disable \
    -v "$PWD":/src \
    -v linux-image-writer-cargo-registry:/opt/cargo/registry \
    linux-image-writer-appimage-build "$@"
}

# Regenerate data/THIRD-PARTY-LICENSES.txt (after Cargo.lock, about.toml or
# the template changed); review the diff, then commit it.
run build-aux/rust-licenses/generate.sh

# Check that data/THIRD-PARTY-LICENSES.txt is current; changes nothing.
run build-aux/rust-licenses/generate.sh --check
```

```
generate.sh [--check] [--output FILE] [--manifest FILE] [--binary FILE]
```

| Option | Effect |
|---|---|
| (none) | Generate, check, and write `data/THIRD-PARTY-LICENSES.txt` (left untouched if it is already identical) |
| `--check` | Generate and check, then **fail unless `data/THIRD-PARTY-LICENSES.txt` is byte for byte the generated document**. Writes nothing in `data/` |
| `--output FILE` | Also keep the generated document as `FILE` |
| `--manifest FILE` | Write `rust-license-manifest.txt` to `FILE` |
| `--binary FILE` | Check the built GUI's embedded source paths against the crates covered |

Nothing is written unless every check passes. The AppImage build runs
`generate.sh --check --output … --manifest … --binary …`.

### A stale document

`data/THIRD-PARTY-LICENSES.txt` is stale when anything it is generated from
changed without regenerating it: `Cargo.lock` (a crate added, removed or
updated), `about.toml`, the template, a crate's licence files, or the
cargo-about release. `--check` then fails with

```
generate.sh: data/THIRD-PARTY-LICENSES.txt is STALE: it is not what Cargo.lock, about.toml and the template generate now.
    <sha256>  generated now
    <sha256>  data/THIRD-PARTY-LICENSES.txt
First differences (- repository, + generated):
    ...
generate.sh: error: THIRD-PARTY-LICENSES.txt must be regenerated: run build-aux/rust-licenses/generate.sh (without --check) in the AppImage build container, review the change, and commit data/THIRD-PARTY-LICENSES.txt
```

and so does the AppImage build. Never edit the file by hand: any byte that
is not generated makes the check fail.

### Before a Flatpak build, and in CI

The Flatpak build cannot check the document itself (it does not run
cargo-about: it builds offline from `cargo-sources.json`, and its SDK has no
licence tooling). So before building the Flatpak, and in any CI job that
builds it, run the check first:

```sh
run build-aux/rust-licenses/generate.sh --check   # must pass
flatpak run org.flatpak.Builder --user --install --force-clean \
  build-dir build-aux/flatpak/io.github.pc_freedom.linux-image-writer.yml
```

A future CI workflow runs `generate.sh --check` on every change to
`Cargo.lock`, `build-aux/rust-licenses/` or `data/THIRD-PARTY-LICENSES.txt`.

## Which crates

The crates in the document are those Cargo itself (`cargo tree`) says the GUI
depends on as **normal dependencies**, with `--features gui`, for
`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, the app excepted.
That includes **procedural macros** and what they use: they run at compile
time, and the code they generate is compiled into the program, so they are
kept rather than argued away. `rust-license-manifest.txt` marks each crate
`linked` or `proc-macro`.

Not in the document, and listed in the manifest's `[excluded]` section with
the reason:

- **build-dependencies** (`cc`, `pkg-config`, `system-deps`, ...): they run
  on the build machine, in build scripts, and are not part of the program;
- **dev-dependencies**: tests only;
- crates for **other platforms** (Windows, macOS, WebAssembly) or for
  features the GUI does not enable.

cargo-about's view of the package is per package, not per binary: the
library and the development CLI of the same package bring in a few crates
(`ctrlc`, for one) the GUI may not use. They are kept: a notice too many is
harmless, one too few is not.

## Which licences

`about.toml` accepts **MIT** for every crate, and nothing else globally.
Every crate in the graph offers MIT, alone or as one choice of an "OR"
expression (`MIT OR Apache-2.0`, `Unlicense OR MIT`, ...); where it is a
choice, the crate is used under MIT, whose only condition is to carry the
notice and the licence text. MIT is compatible with the app's
GPL-3.0-or-later.

Licences only one crate needs are accepted for that crate only:

| Crate | Also accepted | Why |
|---|---|---|
| `liblzma-sys` | 0BSD | With the `static` feature it compiles XZ Utils' liblzma into the program (`xz/src/liblzma`, `xz/src/common/tuklib_*.c`, all `SPDX-License-Identifier: 0BSD`) |
| `regex-syntax` | Unicode-DFS-2016 | Its Unicode tables come from the Unicode Character Database (`src/unicode_tables/LICENSE-UNICODE`); linked through `gettext-rs` → `locale_config` → `regex` |
| `unicode-ident` | Unicode-3.0 | Its Unicode tables, as its own licence expression says |
| `linux-image-writer` | GPL-3.0-or-later | The app itself, the root of the graph; left out of the document (its `LICENSE` is shipped on its own) |

The checks fail if `about.toml` accepts a licence that no crate is used
under, so the list cannot grow "just in case".

## Clarifications

A clarification in `about.toml` states a crate's licence expression and the
files that are its licence statement, each with its SHA-256; those files,
and only those, then go into the document. They are used for:

- crates that bundle code under another licence than their own expression
  says (`liblzma-sys`, `regex-syntax`): the expression is widened to say so,
  and the other licence's files are included;
- crates that carry third-party notices for code they contain
  (`atomic-waker`, `futures-lite`, `parking`: `LICENSE-THIRD-PARTY`;
  `tracing-core`: `src/spin/LICENSE`);
- crates whose licence file has no copyright line but which have a separate
  copyright statement (`rustix`, `linux-raw-sys` and the gtk-rs crates:
  `COPYRIGHT`).

Whole files only (no `start` / `end`), so the checksum covers all of the
text. When a crate is updated and one of its files changes, the checksum no
longer matches: **cargo-about itself would only log a warning and silently
fall back to guessing**, so `generate.sh` fails instead. Read the new file,
then update the checksum (`check.py` prints the new one).

## The document

Plain text (`THIRD-PARTY-LICENSES.txt`): it is read with any viewer from the
extracted AppImage or the installed Flatpak, sits naturally beside the other
licence documents (Debian `copyright` files, `LICENSE`), and two versions
can be compared with `diff`. HTML would need a browser and escaping, and
adds nothing a reader of licence texts needs.

It lists every crate (name, version, licence expression, crates.io page,
repository, authors from its `Cargo.toml`), then every distinct licence text
with the crates that use it. Texts come from the crates' own files, so they
carry the copyright lines the crates provide; many MIT files have none, which
is why the authors are listed too. The app's own licence is not in it.

It contains no time, no absolute path, no temporary name and nothing random:
the same `Cargo.lock`, `about.toml`, template and cargo-about give the same
bytes (checked by generating it twice, and from an empty crate cache).

## Checks

`check.py` takes the distributed crates from Cargo (`cargo tree`), never
from cargo-about, and fails, naming each problem, if:

- cargo-about wrote anything to its error output (warnings included);
- the crates cargo-about covered are not exactly the distributed crates, a
  version differs from `Cargo.lock`, or a crate is not from crates.io with a
  checksum;
- a crate has no licence text, or a text that is not one of its own files
  (cargo-about's quiet fallback to a canonical text, which has no copyright
  notice);
- a crate is used under a licence `about.toml` does not accept for it, or
  under licences that do not satisfy its expression; or cargo-about used an
  expression other than the crate's `Cargo.toml` (normalized: Cargo's old
  `MIT/Apache-2.0` means `MIT OR Apache-2.0`) without a clarification;
- a clarified file's SHA-256 differs, a clarification was not applied, or
  one is for a crate not in the graph; or a clarification uses `start` /
  `end`;
- `about.toml` accepts a licence no crate is used under;
- the document does not list exactly the distributed crates with their
  versions and licences, lacks a licence text or its list of users, or
  contains a path of the build machine;
- given the binary: it contains source paths
  (`…/index.crates.io-…/<crate>-<version>/…`, left by panic messages) of a
  crate the document does not cover. This is evidence from the binary
  itself; most crates leave no such path, so it can show a crate missing but
  not prove one present.

`rust-license-manifest.txt` records the inputs (SHA-256 of `Cargo.lock`,
`about.toml` and the template; cargo-about and cargo versions), a summary,
then one row per distributed crate (role, declared licence, the licence it is
used under, its licence files, whether it is clarified, whether the binary
shows its source paths, its `Cargo.lock` checksum, its repository), every
licence text's SHA-256, every clarified file, and every excluded `Cargo.lock`
package with the reason. In a fixed order, without timestamps or paths:
compare two with `diff`.

## Updating

- **`Cargo.lock` changed** (a crate added, removed or updated): regenerate
  (`generate.sh`), review the change to `data/THIRD-PARTY-LICENSES.txt`, and
  commit it with `Cargo.lock` (and `cargo-sources.json` for the Flatpak).
  A crate whose expression MIT does not satisfy, a changed clarified file,
  or a new licence makes it fail; review the crate's licence files, then
  update `about.toml`.
- **cargo-about**: choose a tagged release, check the archive's SHA-256
  against the digest GitHub publishes, record the executable's SHA-256, and
  change every line of `cargo-about.lock` at once; rebuild the container
  image, regenerate, and compare the document and the manifest with the
  previous ones.

## In the packagings

- **AppImage** (`build-aux/appimage/build.sh`): runs `generate.sh --check`
  (with the binary), compares the generated document with
  `data/THIRD-PARTY-LICENSES.txt` once more, and installs the latter as
  `usr/share/doc/linux-image-writer/THIRD-PARTY-LICENSES.txt`;
  `check-appdir.sh` checks the copy in the AppDir, and in the extracted
  AppImage, against it.
- **Flatpak** (`build-aux/flatpak/…yml`): copies `LICENSE` and
  `data/THIRD-PARTY-LICENSES.txt` into `/app/share/doc/linux-image-writer/`;
  nothing else. Check the document first (above).
