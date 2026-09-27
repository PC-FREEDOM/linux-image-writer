// Image preparation for a write (orchestration, Core layer): turns an
// image `image_source::open_image` already opened into the source the write
// pipeline reads from. Moved unchanged from `main.rs` (Phase 3A-1); the
// target-side re-check after Preflight stays with its caller.
//
// Also image inspection (`inspect_image`): what an image is, for a UI to
// show before any operation, classified by the same `open_image` and judged
// by the same Verify rule the write uses.

use std::path::Path;

use crate::execution::core;
use crate::execution::write_job::verify_mode_supported;
use crate::image_source::{
    self, CompressionFormat, ImageSource, ImageSourceAccess, ImageSourceError,
};

// Why a compressed image was not accepted for writing. Every case stops
// before the target is opened.
#[derive(Debug)]
pub enum CompressedImageRejection {
    // Quick Verify needs random access, which a compressed image cannot
    // provide; refused before Preflight (and before any confirmation).
    QuickVerifyUnsupported(image_source::CompressionFormat),
    // Preflight did not validate the image (includes cancellation).
    Preflight(image_source::compressed::PreflightError),
    // Preflight succeeded, but the file is no longer in the state it was
    // opened in (compared with the snapshot `open_image` took).
    SourceChanged(image_source::source_identity::SourceChanged),
}

// Turns an opened compressed image into the source the write pipeline reads
// from: refuses Quick Verify first (L1), then validates the whole stream
// (Preflight, bounded by `max_logical_size` -- the target's capacity -- and
// cancellable via `is_cancelled`), then confirms the file did not change
// while it was being validated. The file is the one `open_image` opened; it
// is moved, never re-opened.
pub(crate) fn prepare_compressed_image(
    compressed: image_source::CompressedImageFile,
    verify_mode: core::VerifyMode,
    max_logical_size: u64,
    is_cancelled: impl FnMut() -> bool,
    on_progress: impl FnMut(image_source::compressed::PreflightProgress),
) -> Result<image_source::compressed::CompressedImageSource, CompressedImageRejection> {
    if !verify_mode_supported(verify_mode, compressed.access()) {
        return Err(CompressedImageRejection::QuickVerifyUnsupported(
            compressed.format(),
        ));
    }

    let options = image_source::compressed::PreflightOptions { max_logical_size };
    let preflighted = compressed
        .preflight(options, is_cancelled, on_progress)
        .map_err(CompressedImageRejection::Preflight)?;

    preflighted
        .revalidate_identity()
        .map_err(CompressedImageRejection::SourceChanged)?;

    Ok(image_source::compressed::CompressedImageSource::new(
        preflighted,
    ))
}

// ---- Inspection: what an image is, before any operation ----

// A read-only description of an image file for a UI: opened, classified and
// closed again by `inspect_image`. It holds no file, source or selection and
// nothing accepts it -- a write opens the image itself and applies Source
// Identity from its own open, so nothing here vouches for the file later.
/// What an image file is, as a write would classify it: for display before
/// an operation. It grants nothing and is never used by the operation, which
/// opens and checks the image itself when it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    file_size: u64,
    compression: Option<CompressionFormat>,
    access: ImageSourceAccess,
    logical_size: Option<u64>,
}

/// How an image's data can be read back, as far as its format allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAccess {
    /// Any offset can be read directly (an uncompressed image).
    RandomAccess,
    /// The data can only be replayed from its start (a gzip or xz image).
    SequentialReplay,
}

/// Whether a [`VerifyMode`](crate::VerifyMode) can be used with an image's
/// format. It does not promise that Verify will succeed on a particular
/// device: whether its direct (O_DIRECT) reads work is only known when
/// Verify starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyAvailability {
    Available,
    Unavailable(VerifyUnavailableReason),
}

/// Why a Verify mode cannot be used with an image's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyUnavailableReason {
    /// The mode reads sampled windows at arbitrary offsets (Quick), and the
    /// image can only be replayed from its start.
    NeedsRandomAccess,
}

// Part of the library's Production API; unused by the CLI binary.
#[allow(dead_code)]
impl ImageInfo {
    /// The size of the file itself (for a compressed image, the compressed
    /// size).
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// The compression detected from the file's content (never its name);
    /// `None` for an uncompressed image.
    pub fn compression(&self) -> Option<CompressionFormat> {
        self.compression
    }

    /// How the image's data can be read back.
    pub fn access(&self) -> ImageAccess {
        match self.access {
            ImageSourceAccess::RandomAccess => ImageAccess::RandomAccess,
            ImageSourceAccess::SequentialReplay => ImageAccess::SequentialReplay,
        }
    }

    /// The number of bytes that would be written, when it is known without
    /// decoding: an uncompressed image's size. `None` for a compressed
    /// image, whose size is only established by the operation's full
    /// validation (Preflight).
    pub fn logical_size(&self) -> Option<u64> {
        self.logical_size
    }

    /// Whether `mode` can be used with this image's format, by the same
    /// rule the operation enforces.
    pub fn verify_availability(&self, mode: core::VerifyMode) -> VerifyAvailability {
        if verify_mode_supported(mode, self.access) {
            VerifyAvailability::Available
        } else {
            VerifyAvailability::Unavailable(VerifyUnavailableReason::NeedsRandomAccess)
        }
    }
}

// Opens the image once with `open_image` -- the same classification by
// content (and the same refusals) the write uses -- reads what it found and
// closes it. A compressed image is not decoded: its logical size stays
// unknown until an operation's Preflight.
/// Describes the image at `path` for display: its format, size, access and
/// which Verify modes its format allows. Read-only; an unsupported or
/// unreadable file is the same error the write operation would report.
#[allow(dead_code)]
pub fn inspect_image(path: impl AsRef<Path>) -> Result<ImageInfo, ImageSourceError> {
    Ok(match image_source::open_image(path.as_ref())? {
        image_source::OpenedImage::Raw(source) => ImageInfo {
            file_size: source.logical_size(),
            compression: None,
            access: source.access(),
            logical_size: Some(source.logical_size()),
        },
        image_source::OpenedImage::Compressed(compressed) => ImageInfo {
            file_size: compressed.compressed_size(),
            compression: Some(compressed.format()),
            access: compressed.access(),
            logical_size: None,
        },
    })
}

#[cfg(test)]
mod tests {
    // ---------------------------------------------------------------------
    // gzip preparation (Quick L1, Preflight, post-Preflight source check).
    // All of this happens before the target is opened, so a rejection here
    // cannot touch the target.
    // ---------------------------------------------------------------------

    use super::{CompressedImageRejection, prepare_compressed_image};
    use crate::execution::core::VerifyMode;
    use crate::image_source::compressed::{PreflightError, PreflightProgress};
    use crate::image_source::{
        CompressedImageFile, ImageSource, ImageSourceAccess, OpenedImage, open_image,
    };

    // A temporary `.img.gz` file, removed when dropped.
    struct TempGzip(std::path::PathBuf);

    impl Drop for TempGzip {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn gzip_bytes(payload: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    fn temp_gzip(tag: &str, contents: &[u8]) -> TempGzip {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "linux-usb-writer-main-test-{tag}-{}-{id}.img.gz",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        TempGzip(path)
    }

    fn open_gzip(file: &TempGzip) -> CompressedImageFile {
        match open_image(&file.0) {
            Ok(OpenedImage::Compressed(compressed)) => compressed,
            other => panic!("expected a compressed image, got {other:?}"),
        }
    }

    fn payload() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 253) as u8).collect()
    }

    // None and Full: the whole stream is validated, the source reports the
    // decoded size and sequential access, and progress ends at the exact
    // totals.
    #[test]
    fn prepare_compressed_image_accepts_valid_gzip_for_none_and_full() {
        let data = payload();
        let compressed = gzip_bytes(&data);
        for mode in [VerifyMode::None, VerifyMode::Full] {
            let file = temp_gzip("prepare-ok", &compressed);
            let mut last = None;
            let source = prepare_compressed_image(
                open_gzip(&file),
                mode,
                data.len() as u64,
                || false,
                |progress| last = Some(progress),
            )
            .unwrap_or_else(|rejection| panic!("{mode:?}: {rejection:?}"));

            assert_eq!(source.logical_size(), data.len() as u64);
            assert_eq!(source.access(), ImageSourceAccess::SequentialReplay);
            source.revalidate_identity().unwrap();
            let last = last.expect("progress reported");
            assert_eq!(last.logical_produced, data.len() as u64);
            assert_eq!(last.compressed_consumed, compressed.len() as u64);
        }
    }

    // L1: Quick is refused before any validation work (no cancellation
    // poll, no progress) -- and never silently turned into Full or None.
    #[test]
    fn prepare_compressed_image_refuses_quick_before_validating() {
        let file = temp_gzip("prepare-quick", &gzip_bytes(&payload()));
        let result = prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::Quick,
            u64::MAX,
            || panic!("Quick must be refused before Preflight polls for cancellation"),
            |_| panic!("Quick must be refused before Preflight reports progress"),
        );
        assert!(matches!(
            result,
            Err(CompressedImageRejection::QuickVerifyUnsupported(
                crate::image_source::CompressionFormat::Gzip
            ))
        ));
    }

    fn expect_preflight_error(
        result: Result<
            crate::image_source::compressed::CompressedImageSource,
            CompressedImageRejection,
        >,
    ) -> PreflightError {
        match result {
            Err(CompressedImageRejection::Preflight(error)) => error,
            Err(other) => panic!("expected a Preflight rejection, got {other:?}"),
            Ok(_) => panic!("expected a Preflight rejection, got a source"),
        }
    }

    // Cancellation during Preflight is reported as such (the CLI maps it to
    // the Cancelled exit).
    #[test]
    fn prepare_compressed_image_can_be_cancelled() {
        let file = temp_gzip("prepare-cancel", &gzip_bytes(&payload()));
        let error = expect_preflight_error(prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::None,
            u64::MAX,
            || true,
            |_| {},
        ));
        assert!(matches!(error, PreflightError::Cancelled));
    }

    // Corrupt, truncated and oversized images are rejected with their typed
    // Preflight reason; the size limit is the one passed in (the target's
    // capacity) and allows an image of exactly that size.
    #[test]
    fn prepare_compressed_image_rejects_bad_images_with_typed_reasons() {
        let data = payload();
        let good = gzip_bytes(&data);

        let mut corrupt = good.clone();
        let crc = corrupt.len() - 8;
        corrupt[crc] ^= 0xFF;
        let file = temp_gzip("prepare-corrupt", &corrupt);
        let error = expect_preflight_error(prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::None,
            u64::MAX,
            || false,
            |_| {},
        ));
        assert!(matches!(error, PreflightError::Corrupt(_)), "{error:?}");

        let file = temp_gzip("prepare-truncated", &good[..good.len() / 2]);
        let error = expect_preflight_error(prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::Full,
            u64::MAX,
            || false,
            |_| {},
        ));
        assert!(matches!(error, PreflightError::Incomplete(_)), "{error:?}");

        let file = temp_gzip("prepare-oversized", &good);
        let limit = data.len() as u64 - 1;
        let error = expect_preflight_error(prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::None,
            limit,
            || false,
            |_| {},
        ));
        assert!(
            matches!(error, PreflightError::LogicalSizeLimitExceeded { limit: l } if l == limit),
            "{error:?}"
        );

        let file = temp_gzip("prepare-exact-limit", &good);
        assert!(
            prepare_compressed_image(
                open_gzip(&file),
                VerifyMode::None,
                data.len() as u64,
                || false,
                |_| {},
            )
            .is_ok()
        );
    }

    // The file changes while Preflight runs (at its final progress report):
    // validation itself succeeds, but the source is refused because it is
    // compared with the snapshot `open_image` took.
    #[test]
    fn prepare_compressed_image_refuses_a_source_changed_during_preflight() {
        let compressed = gzip_bytes(&payload());
        let file = temp_gzip("prepare-changed", &compressed);
        let path = file.0.clone();
        let data_len = payload().len() as u64;
        let result = prepare_compressed_image(
            open_gzip(&file),
            VerifyMode::None,
            u64::MAX,
            || false,
            |progress: PreflightProgress| {
                if progress.logical_produced == data_len {
                    use std::os::unix::fs::FileExt as _;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let same = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                    same.write_all_at(&compressed[..16], 0).unwrap();
                }
            },
        );
        assert!(
            matches!(result, Err(CompressedImageRejection::SourceChanged(_))),
            "{result:?}"
        );
    }

    // ---------------------------------------------------------------------
    // xz preparation: the same `prepare_compressed_image` as gzip -- the
    // format only picks the decoder inside Preflight.
    // ---------------------------------------------------------------------

    // A temporary `.img.xz` file, removed when dropped.
    struct TempXz(std::path::PathBuf);

    impl Drop for TempXz {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn xz_bytes(payload: &[u8], check: liblzma::stream::Check) -> Vec<u8> {
        use std::io::Write as _;
        let stream = liblzma::stream::Stream::new_easy_encoder(0, check).unwrap();
        let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    fn crc64_xz(payload: &[u8]) -> Vec<u8> {
        xz_bytes(payload, liblzma::stream::Check::Crc64)
    }

    fn temp_xz(tag: &str, contents: &[u8]) -> TempXz {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "linux-usb-writer-main-test-{tag}-{}-{id}.img.xz",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        TempXz(path)
    }

    fn open_xz(file: &TempXz) -> CompressedImageFile {
        match open_image(&file.0) {
            Ok(OpenedImage::Compressed(compressed)) => {
                assert_eq!(
                    compressed.format(),
                    crate::image_source::CompressionFormat::Xz
                );
                compressed
            }
            other => panic!("expected a compressed image, got {other:?}"),
        }
    }

    fn prepare_xz(
        tag: &str,
        contents: &[u8],
        mode: VerifyMode,
        limit: u64,
    ) -> Result<crate::image_source::compressed::CompressedImageSource, CompressedImageRejection>
    {
        let file = temp_xz(tag, contents);
        prepare_compressed_image(open_xz(&file), mode, limit, || false, |_| {})
    }

    // Rewrites a single stream's Check ID (header and footer, CRC32s
    // recomputed; same check-field size) or its first Block's LZMA2
    // dictionary byte (Block Header CRC32 recomputed). Fixed .xz offsets of
    // a stream this test just encoded.
    fn xz_patched(stream: &[u8], check_id: Option<u8>, dictionary_byte: Option<u8>) -> Vec<u8> {
        let crc32 = |bytes: &[u8]| {
            let mut crc = flate2::Crc::new();
            crc.update(bytes);
            crc.sum().to_le_bytes()
        };
        let mut patched = stream.to_vec();
        if let Some(id) = check_id {
            let footer = patched.len() - 12;
            patched[7] = id;
            patched[footer + 9] = id;
            let header_crc = crc32(&patched[6..8]);
            patched[8..12].copy_from_slice(&header_crc);
            let footer_crc = crc32(&patched[footer + 4..footer + 10]);
            patched[footer..footer + 4].copy_from_slice(&footer_crc);
        }
        if let Some(byte) = dictionary_byte {
            let header_len = (patched[12] as usize + 1) * 4;
            assert_eq!(&patched[13..16], &[0x00, 0x21, 0x01], "one LZMA2 filter");
            patched[16] = byte;
            let crc_at = 12 + header_len - 4;
            let crc = crc32(&patched[12..crc_at]);
            patched[crc_at..crc_at + 4].copy_from_slice(&crc);
        }
        patched
    }

    // None and Full: a single stream, and concatenated streams with Stream
    // Padding, are validated to their exact decoded size; the source is the
    // same sequential-replay `CompressedImageSource` gzip produces.
    #[test]
    fn prepare_compressed_image_accepts_valid_xz_for_none_and_full() {
        let data = payload();
        let (half_a, half_b) = data.split_at(data.len() / 2);
        let single = xz_bytes(&data, liblzma::stream::Check::Sha256);
        let concatenated = [
            crc64_xz(half_a),
            vec![0u8; 8],
            xz_bytes(half_b, liblzma::stream::Check::Crc32),
            vec![0u8; 4],
        ]
        .concat();

        for (name, compressed) in [("single", &single), ("concatenated", &concatenated)] {
            for mode in [VerifyMode::None, VerifyMode::Full] {
                let file = temp_xz(&format!("prepare-xz-{name}"), compressed);
                let mut last = None;
                let source = prepare_compressed_image(
                    open_xz(&file),
                    mode,
                    data.len() as u64,
                    || false,
                    |progress| last = Some(progress),
                )
                .unwrap_or_else(|rejection| panic!("{name} {mode:?}: {rejection:?}"));

                assert_eq!(source.logical_size(), data.len() as u64);
                assert_eq!(source.access(), ImageSourceAccess::SequentialReplay);
                source.revalidate_identity().unwrap();
                let last = last.expect("progress reported");
                assert_eq!(last.logical_produced, data.len() as u64);
                assert_eq!(last.compressed_consumed, compressed.len() as u64);
            }
        }
    }

    // L1 for xz: Quick is refused before any validation work, and never
    // turned into Full or None.
    #[test]
    fn prepare_compressed_image_refuses_quick_for_xz_before_validating() {
        let file = temp_xz("prepare-xz-quick", &crc64_xz(&payload()));
        let result = prepare_compressed_image(
            open_xz(&file),
            VerifyMode::Quick,
            u64::MAX,
            || panic!("Quick must be refused before Preflight polls for cancellation"),
            |_| panic!("Quick must be refused before Preflight reports progress"),
        );
        assert!(matches!(
            result,
            Err(CompressedImageRejection::QuickVerifyUnsupported(
                crate::image_source::CompressionFormat::Xz
            ))
        ));
    }

    // Bad xz images are refused by Preflight with their typed reason --
    // before the target is opened.
    #[test]
    fn prepare_compressed_image_rejects_bad_xz_with_typed_reasons() {
        let data = payload();
        let good = crc64_xz(&data);
        let reject = |tag: &str, contents: &[u8], limit: u64| {
            expect_preflight_error(prepare_xz(tag, contents, VerifyMode::Full, limit))
        };

        let mut corrupt = good.clone();
        let footer_crc = corrupt.len() - 12;
        corrupt[footer_crc] ^= 0xFF;
        let error = reject("prepare-xz-corrupt", &corrupt, u64::MAX);
        assert!(matches!(error, PreflightError::Corrupt(_)), "{error:?}");

        let error = reject("prepare-xz-truncated", &good[..good.len() / 2], u64::MAX);
        assert!(matches!(error, PreflightError::Incomplete(_)), "{error:?}");

        let none = xz_bytes(&data, liblzma::stream::Check::None);
        let error = reject("prepare-xz-none", &none, u64::MAX);
        assert!(
            matches!(error, PreflightError::IntegrityCheckMissing),
            "{error:?}"
        );

        let reserved = xz_patched(&good, Some(5), None);
        let error = reject("prepare-xz-reserved", &reserved, u64::MAX);
        assert!(
            matches!(error, PreflightError::UnsupportedIntegrityCheck),
            "{error:?}"
        );

        let huge_dictionary = xz_patched(&good, None, Some(40));
        let error = reject("prepare-xz-memlimit", &huge_dictionary, u64::MAX);
        assert!(
            matches!(error, PreflightError::DecoderMemoryLimitExceeded { .. }),
            "{error:?}"
        );

        let limit = data.len() as u64 - 1;
        let error = reject("prepare-xz-oversized", &good, limit);
        assert!(
            matches!(error, PreflightError::LogicalSizeLimitExceeded { limit: l } if l == limit),
            "{error:?}"
        );
        assert!(
            prepare_xz(
                "prepare-xz-exact",
                &good,
                VerifyMode::None,
                data.len() as u64
            )
            .is_ok()
        );
    }

    // Post-Preflight source check for xz: the file changes during Preflight
    // (at its final progress report) and the source is refused.
    #[test]
    fn prepare_compressed_image_refuses_an_xz_source_changed_during_preflight() {
        let compressed = crc64_xz(&payload());
        let file = temp_xz("prepare-xz-changed", &compressed);
        let path = file.0.clone();
        let data_len = payload().len() as u64;
        let result = prepare_compressed_image(
            open_xz(&file),
            VerifyMode::Full,
            u64::MAX,
            || false,
            |progress: PreflightProgress| {
                if progress.logical_produced == data_len
                    && progress.compressed_consumed == compressed.len() as u64
                {
                    use std::os::unix::fs::FileExt as _;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let same = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                    same.write_all_at(&compressed[..16], 0).unwrap();
                }
            },
        );
        assert!(
            matches!(result, Err(CompressedImageRejection::SourceChanged(_))),
            "{result:?}"
        );
    }

    // ---------------------------------------------------------------------
    // Inspection: the same classification as `open_image`, and the same
    // Verify rule the write enforces.
    // ---------------------------------------------------------------------

    mod inspection {
        use super::super::{
            ImageAccess, VerifyAvailability, VerifyUnavailableReason, inspect_image,
        };
        use super::{CompressedImageRejection, prepare_compressed_image};
        use crate::execution::core::VerifyMode;
        use crate::image_source::{
            CompressionFormat, ImageSourceError, OpenedImage, UnsupportedCompression, open_image,
        };
        use crate::orchestration::test_support::{gzip, payload, temp_image, xz};

        const MODES: [VerifyMode; 3] = [VerifyMode::None, VerifyMode::Quick, VerifyMode::Full];

        #[test]
        fn a_raw_image_is_described_from_its_one_open() {
            let data = payload();
            let image = temp_image("inspect-raw", "img", &data);
            let info = inspect_image(&image.0).unwrap();

            assert_eq!(info.file_size(), data.len() as u64);
            assert_eq!(info.compression(), None);
            assert_eq!(info.access(), ImageAccess::RandomAccess);
            assert_eq!(info.logical_size(), Some(data.len() as u64));
            for mode in MODES {
                assert_eq!(
                    info.verify_availability(mode),
                    VerifyAvailability::Available
                );
            }
            assert!(matches!(open_image(&image.0), Ok(OpenedImage::Raw(_))));
        }

        // A compressed image is recognised by content, not decoded: its
        // logical size stays unknown, and Quick is unavailable -- the same
        // answer the write gives when it refuses Quick before Preflight.
        #[test]
        fn a_compressed_image_is_described_without_decoding() {
            let data = payload();
            for (format, bytes, extension) in [
                (CompressionFormat::Gzip, gzip(&data), "img.gz"),
                (CompressionFormat::Xz, xz(&data), "img.xz"),
            ] {
                let image = temp_image("inspect-compressed", extension, &bytes);
                let info = inspect_image(&image.0).unwrap();

                assert_eq!(info.file_size(), bytes.len() as u64);
                assert_eq!(info.compression(), Some(format));
                assert_eq!(info.access(), ImageAccess::SequentialReplay);
                assert_eq!(info.logical_size(), None);
                assert_eq!(
                    info.verify_availability(VerifyMode::Quick),
                    VerifyAvailability::Unavailable(VerifyUnavailableReason::NeedsRandomAccess)
                );

                for mode in MODES {
                    let Ok(OpenedImage::Compressed(compressed)) = open_image(&image.0) else {
                        panic!("{format:?} must open as compressed");
                    };
                    assert_eq!(compressed.format(), format);
                    let prepared =
                        prepare_compressed_image(compressed, mode, u64::MAX, || false, |_| {});
                    let refused = matches!(
                        prepared,
                        Err(CompressedImageRejection::QuickVerifyUnsupported(refused))
                            if refused == format
                    );
                    assert_eq!(
                        refused,
                        info.verify_availability(mode) != VerifyAvailability::Available,
                        "{format:?} {mode:?}"
                    );
                    if !refused {
                        assert!(prepared.is_ok(), "{format:?} {mode:?}");
                    }
                }
            }
        }

        // Refusals are `open_image`'s own, unchanged.
        #[test]
        fn refusals_are_the_writes_own() {
            let cases: [(&[u8], UnsupportedCompression); 5] = [
                (
                    &[0x28, 0xB5, 0x2F, 0xFD, 0, 0],
                    UnsupportedCompression::Zstd,
                ),
                (b"BZh91AY", UnsupportedCompression::Bzip2),
                (&[0x50, 0x4B, 0x03, 0x04, 0, 0], UnsupportedCompression::Zip),
                (
                    &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C],
                    UnsupportedCompression::SevenZip,
                ),
                (&[0x04, 0x22, 0x4D, 0x18, 0, 0], UnsupportedCompression::Lz4),
            ];
            for (bytes, kind) in cases {
                let image = temp_image("inspect-unsupported", "img", bytes);
                assert!(
                    matches!(
                        inspect_image(&image.0),
                        Err(ImageSourceError::UnsupportedFormat(found)) if found == kind
                    ),
                    "{kind:?}"
                );
                assert!(matches!(
                    open_image(&image.0),
                    Err(ImageSourceError::UnsupportedFormat(found)) if found == kind
                ));
            }

            // A name promising a compression the content does not have.
            let data = payload();
            for (bytes, extension, expected) in [
                (data.clone(), "img.gz", CompressionFormat::Gzip),
                (gzip(&data), "img.xz", CompressionFormat::Xz),
            ] {
                let image = temp_image("inspect-mismatch", extension, &bytes);
                assert!(matches!(
                    inspect_image(&image.0),
                    Err(ImageSourceError::ExtensionMismatch { expected: found }) if found == expected
                ));
                assert!(matches!(
                    open_image(&image.0),
                    Err(ImageSourceError::ExtensionMismatch { expected: found }) if found == expected
                ));
            }

            // Not a regular file; a missing file.
            let directory = std::env::temp_dir();
            assert!(matches!(
                inspect_image(&directory),
                Err(ImageSourceError::NotRegularFile)
            ));
            let missing = temp_image("inspect-missing", "img", b"");
            let missing_path = missing.0.clone();
            drop(missing);
            assert!(matches!(
                inspect_image(&missing_path),
                Err(ImageSourceError::Io(_))
            ));
        }
    }
}
