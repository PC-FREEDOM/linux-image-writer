// Image preparation for a write (orchestration, Core layer): turns an
// image `image_source::open_image` already opened into the source the write
// pipeline reads from. Moved unchanged from `main.rs` (Phase 3A-1); the
// target-side re-check after Preflight stays with its caller.

use crate::execution::core;
use crate::image_source;

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
    if verify_mode == core::VerifyMode::Quick {
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
}
