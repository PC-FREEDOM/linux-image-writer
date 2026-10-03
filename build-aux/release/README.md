# Releases

`release.sh` builds every artifact of a Linux Image Writer release from one
commit, and checks that they all trace back to it. It is the only way to
produce artifacts with a release's name; everything else makes test builds.
Users' instructions for the AppImage are in `docs/AppImage.md`.

It orchestrates the existing scripts and adds nothing AppImage-specific:

| Step | Done by |
|---|---|
| Version and git state | `release.sh` |
| `SOURCE_DATE_EPOCH` = the commit's time | `release.sh` |
| `data/THIRD-PARTY-LICENSES.txt` is current | `build-aux/rust-licenses/generate.sh --check` |
| The AppImage | `build-aux/appimage/build.sh` |
| Its corresponding source archive | `build-aux/appimage/sources.sh` (and `--check`) |
| The application's source archive | `git archive` of the tag, `gzip -n -9` |
| SHA-256 files, traceability checks, `RELEASE-MANIFEST.txt` | `release.sh` |

It runs in the AppImage build container (`build-aux/appimage/Containerfile`,
which has git, gzip, zstd, cargo-about and the pinned toolchain), the same
way on a workstation and in CI: no CI-specific logic.

```sh
run() {
  podman run --rm --security-opt label=disable \
    -v "$PWD":/src \
    -v linux-image-writer-cargo-registry:/opt/cargo/registry \
    linux-image-writer-appimage-build "$@"
}
run build-aux/release/release.sh --release 0.1.1 --validate-only   # checks only
run build-aux/release/release.sh --release 0.1.1                  # official
run build-aux/release/release.sh --test                            # test build
```

## Official and test builds

| | `--release VERSION` | `--test` |
|---|---|---|
| Source | the annotated tag `vVERSION`, which must be HEAD | HEAD and the working tree as they are |
| Working tree | must be clean (no modified or untracked files; ignored files are fine) | may be modified (recorded in the manifest) |
| Version | `Cargo.toml`, `Cargo.lock` (the app's package) and the newest `<release>` of the MetaInfo must all be VERSION | `<Cargo.toml version>-test` |
| Other tags on HEAD | refused (a release is never built from another release's commit, e.g. v0.1.0's) | not checked |
| Names | `LinuxImageWriter-VERSION-…` | `LinuxImageWriter-<version>-test-…` |
| Traceability | the app's files in the AppImage must equal the commit's | compared with the working tree; the match with the commit is recorded |

`build.sh` and `sources.sh` refuse an `APPIMAGE_VERSION` that does not end in
`-test` unless `release.sh` runs them (`LIW_OFFICIAL_RELEASE=1`, set only
after its checks), so a release's name cannot be produced by accident.

## SOURCE_DATE_EPOCH

Every file time in every artifact is the release commit's own commit time
(`git show -s --format=%ct vVERSION^{commit}`):

- the AppImage: `build.sh` gives it to mksquashfs (`-all-time`,
  `-mkfs-time`);
- the corresponding source archive: `sources.sh` gives it to tar (`--mtime`);
- the application's source archive: `git archive` uses the commit time for
  every entry by itself, and `gzip -n` leaves out the name and time.

So all artifacts of a release carry one time, from the commit, and the same
commit gives the same bytes. `release.sh` always sets it, also for test
builds (HEAD's time). Running `build.sh` or `sources.sh` directly without it
falls back to the Ubuntu snapshot's time -- for development builds only.

## Release assets

Upload exactly these six files (for 0.1.1):

```
LinuxImageWriter-0.1.1-x86_64.AppImage
LinuxImageWriter-0.1.1-x86_64.AppImage.sha256
LinuxImageWriter-0.1.1-source.tar.gz
LinuxImageWriter-0.1.1-source.tar.gz.sha256
LinuxImageWriter-0.1.1-appimage-sources.tar.zst
LinuxImageWriter-0.1.1-appimage-sources.tar.zst.sha256
```

- The application's source archive is attached although GitHub generates
  one for every tag: GitHub's are not guaranteed to stay byte for byte the
  same, so they cannot be verified against a published SHA-256.
- The corresponding source archive is what the licences of the bundled
  libraries (GPL, LGPL, MPL, …) ask to be offered from the same place as
  the AppImage; its `source-manifest.txt` is inside it.
- **Not uploaded**: `RELEASE-MANIFEST.txt` and the build records
  (`records/`: bundle, licence, Rust licence and source manifests). They
  document the build and are reproducible from the tag; the licence texts
  users need are inside the AppImage and the source manifest is inside the
  source archive. Paste the `[assets]` section of `RELEASE-MANIFEST.txt`
  (file, size, SHA-256) and the commit into the release notes instead, so
  the notes tie every asset to the commit.

## RELEASE-MANIFEST.txt

Tab-separated, in a fixed order; its only time is the commit's:

```
kind / version / tag / commit / source-date-epoch / working-tree / appimage-trace
[assets]   file, size, sha256 of each release asset
[records]  sha256 of each build record
```

## Traceability

Before writing the manifest, `release.sh` checks that:

- the tag points at HEAD (official) and no other tag does;
- the application's source archive records that commit (`git
  get-tar-commit-id`), contains `Cargo.lock` and `Cargo.toml`, has every
  entry under `LinuxImageWriter-VERSION/`, and contains no `.git`, `target`
  or build output;
- the AppImage's own files from the repository (AppRun, desktop entry,
  MetaInfo, icon, LICENSE, THIRD-PARTY-LICENSES.txt) are byte for byte that
  commit's (extracted with `--appimage-extract`);
- the corresponding source archive is for the Ubuntu snapshot that commit's
  Containerfile pins, and passes `sources.sh --check`;
- the release directory has exactly the expected files, each matching its
  `.sha256`.

## Releasing v0.1.1: checklist

v0.1.1 is a patch release: since v0.1.0 (tag `v0.1.0`, commit `0059b9b`),
the GUI's style was made GTK 4.14-compatible and the AppImage, the licence
notices and the release tooling were added; the Safety Engine and the
writing behaviour did not change.

1. **Commit the work** so far (packaging, licence notices, tooling), with
   `Cargo.toml` still at 0.1.0. Test builds stay `0.1.0-test`.
2. **Release commit** ("Release 0.1.1") -- the only place the version
   changes:
   - `Cargo.toml`: `version = "0.1.1"`, then `cargo check` (updates the
     app's entry in `Cargo.lock`; nothing else in it changes);
   - `data/io.github.pc_freedom.linux-image-writer.metainfo.xml`: a new
     first `<release version="0.1.1" date="YYYY-MM-DD">` with its notes;
   - `README.md` / `README.ja.md`: Status and Installation (the AppImage,
     linking `docs/AppImage.md`);
   - check: `generate.sh --check` (the document does not list the app
     itself, so it stays current), `cargo test`, and
     `release.sh --release 0.1.1 --validate-only` fails only on the
     missing tag.
3. **Tag**: `git tag -a v0.1.1 -m "Linux Image Writer 0.1.1"` on that
   commit.
4. **Build**: `release.sh --release 0.1.1` (clean checkout of the tag).
   Optionally build again on another machine and compare the SHA-256s.
5. **Push** the commit and the tag; create the GitHub Release from the tag,
   upload the six assets, and put the `[assets]` section and the commit in
   the notes.
6. **Flathub**: bring the Flathub manifest in line with
   `build-aux/flatpak/…yml` -- install `LICENSE` and
   `data/THIRD-PARTY-LICENSES.txt` into `/app/share/doc/linux-image-writer/`
   (added in step 6.8) -- point it at the new tag, and regenerate
   `cargo-sources.json` if `Cargo.lock`'s crates changed. Before building it,
   `generate.sh --check` must pass.
7. Keep the release directory (`build-aux/release/out/LinuxImageWriter-0.1.1/`,
   with `RELEASE-MANIFEST.txt` and `records/`) as the build record.

## Licence document

`data/THIRD-PARTY-LICENSES.txt` is checked twice: up front by `release.sh`
(fail fast) and by `build.sh` before it is copied into the AppImage. If a
release commit changes `Cargo.lock`'s crates without regenerating it, the
release stops with "THIRD-PARTY-LICENSES.txt must be regenerated"
(`build-aux/rust-licenses/README.md`).

## CI

A workflow only needs to build the container image and run the commands
above: `--validate-only` and `--test` on every change, `--release VERSION`
on a pushed `vVERSION` tag, then upload the six assets. Nothing in
`release.sh` depends on the CI.
