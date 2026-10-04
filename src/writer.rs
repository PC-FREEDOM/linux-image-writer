// Writer Core. Deliberately Linux/UDisks2-agnostic: this module knows
// nothing about `DeviceSnapshot`, D-Bus, or the Safety Engine, and never
// decides *whether* a write should happen — only *how* to copy bytes from a
// readable source to a writable target once something else has already
// decided it's safe to do so. Connecting this to a real block device (via
// `linux_access.rs`'s OpenDevice FD) is a separate, later step.
//
// The source/target abstractions are deliberately just `std::io::Read` /
// `std::io::Write` rather than bespoke traits: a plain file and an in-memory
// `Cursor<Vec<u8>>` already implement them, and so will future decompressing
// readers (gzip/xz) and a block-device-backed writer — they only need to
// wrap or implement `Read`/`Write`, not conform to a writer-specific trait.

use std::io::{self, Read, Write};
use std::num::NonZeroU64;

// 1 MiB. Chosen as a reasonable default for chunked copying; not tuned for
// throughput (performance is explicitly out of scope for this PoC).
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

// The write-back window: how far accepted bytes may run ahead of the bytes
// confirmed as written back before the write loop waits for the oldest of
// them (see `write_with_writeback`). It bounds the data still pending when
// the write ends (the final sync) or is cancelled (the drain), and so how
// long either can take: roughly this amount divided by the device's write
// speed. 64 MiB is a starting point to compare against 32 and 128 MiB on
// real devices, not a measured optimum. The one place it is defined.
pub const DEFAULT_WRITEBACK_WINDOW_BYTES: NonZeroU64 = NonZeroU64::new(64 * 1024 * 1024).unwrap();

// Why a write-back request on the target failed. `Unsupported` means the
// request itself is not available for this target (the target says so, e.g.
// the syscall does not exist or the file type does not take it); the write
// loop then falls back to a whole-file data sync. `Failed` is everything
// else, a real failure of the write-back (an I/O error, the device gone):
// it ends the write, it is never taken as "unsupported".
#[derive(Debug)]
pub enum WritebackError {
    // The error is kept for `Debug` output only: the loop acts on the
    // variant, not on the error.
    #[allow(dead_code)]
    Unsupported(io::Error),
    Failed(io::Error),
}

// Write-back control a write target may offer, for `write_with_writeback`.
// Offsets are from the start of the target (= the start of the image).
// The Linux block device implementation lives in `linux_access.rs`; this
// module stays platform-agnostic.
pub trait Writeback {
    // Starts write-back of `[offset, offset + len)` without waiting for it.
    fn start_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError>;

    // Waits until `[offset, offset + len)` is written back, starting
    // write-back for any part of it not yet started. Returns `Ok` only once
    // the whole range was written back.
    fn wait_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError>;

    // Writes back and waits for every byte written so far (`fdatasync`):
    // the fallback when ranged write-back is unsupported.
    fn sync_data(&mut self) -> io::Result<()>;
}

// A borrowed target offers what the target offers (as `&mut W: Write`).
impl<T: Writeback + ?Sized> Writeback for &mut T {
    fn start_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError> {
        (**self).start_writeback(offset, len)
    }

    fn wait_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError> {
        (**self).wait_writeback(offset, len)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        (**self).sync_data()
    }
}

// One progress report from `write_with_writeback`, in the order it happens:
// a chunk accepted, and -- only after a write-back wait or sync succeeded --
// the new end of the written-back range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteLoopProgress {
    Accepted(WriteProgress),
    WrittenBack(WritebackProgress),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePlan {
    pub image_size: u64,
    pub target_size: u64,
    pub chunk_size: usize,
}

impl WritePlan {
    // Pure validation: no I/O, no side effects. `image_size`/`target_size`
    // must be supplied by the caller (e.g. from a source file's metadata and
    // a freshly re-verified DeviceSnapshot.size) — this function only checks
    // that the numbers make sense together.
    pub fn new(image_size: u64, target_size: u64, chunk_size: usize) -> Result<Self, WriteError> {
        if image_size == 0 || target_size == 0 {
            return Err(WriteError::InvalidSize);
        }

        if chunk_size == 0 {
            return Err(WriteError::InvalidChunkSize);
        }

        if image_size > target_size {
            return Err(WriteError::ImageTooLarge);
        }

        Ok(WritePlan {
            image_size,
            target_size,
            chunk_size,
        })
    }
}

// Accepted progress: `bytes_written` is how many bytes `write()` calls have
// returned success for so far -- the bytes the kernel *accepted*, which for
// the buffered block device FD this crate writes through may still be
// waiting in the page cache. It says nothing about write-back to the
// device; that is `WritebackProgress`, a separate value that is never
// derived from this one.
/// Bytes accepted by the kernel so far (`write()` returned success); not
/// necessarily written back to the device yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteProgress {
    pub bytes_written: u64,
    pub total_bytes: u64,
}

// Write-back progress: `completed_bytes` is how many bytes, from the start
// of the image, an explicit sync on the write FD has confirmed as written
// back from the page cache to the device (the sync call returned success
// after covering them). Only a sync result may produce one -- never a
// `write()` return, so it can lag behind `WriteProgress::bytes_written` and
// is simply absent until the first confirmation. Narrower than "durable":
// whether the device's own volatile cache was flushed is the final
// `fsync()`'s business, not this value's.
//
// Produced at three points, each a successful sync: inside the write loop
// when a write-back wait (or the fallback data sync) for the oldest part of
// the window succeeded (`write_with_writeback`); after the final `fsync()`
// (`SyncSucceeded`), which confirms the total; and after a cancelled
// write's `fdatasync()` (`CancelSynced`), which confirms everything
// accepted.
/// Bytes confirmed as written back to the device by an explicit sync, from
/// the start of the image. Never derived from accepted bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritebackProgress {
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

// Fields here are read via the derived Debug impl (for error reporting) and,
// for `Cancelled`, via destructuring in tests — rustc's dead-code lint
// doesn't count either as a use outside of a plain `cargo check` build.
#[allow(dead_code)]
#[derive(Debug)]
pub enum WriteError {
    InvalidSize,
    InvalidChunkSize,
    ImageTooLarge,
    SourceRead(io::Error),
    TargetWrite(io::Error),
    FlushFailed(io::Error),
    // The source ran out of data before `plan.image_size` bytes were read —
    // distinct from `SourceRead`, which is for an actual I/O error. This is
    // never treated as a successful, if-shorter-than-planned write.
    SourceTooShort {
        bytes_written: u64,
    },
    // Carries how much had already been written when cancellation was
    // observed, so the caller always knows the target is left partially
    // written rather than having to guess.
    Cancelled {
        bytes_written: u64,
    },
    // Starting or waiting for write-back (or the fallback data sync) failed
    // after `bytes_written` bytes were accepted: what reached the device is
    // unknown. Never produced for an `Unsupported` write-back request --
    // that switches to the fallback instead.
    Writeback {
        bytes_written: u64,
        error: io::Error,
    },
}

// Reads until `buf` is full or the source is exhausted, retrying on
// `Interrupted`. Unlike `Read::read_exact`, running out of input early is not
// an error here — it's reported as a short read (fewer bytes than `buf.len()`)
// so the caller can decide what that means.
fn read_fully<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;

    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(total)
}

// Copies exactly `plan.image_size` bytes from `source` to `target` in
// `plan.chunk_size` chunks, reporting progress after each chunk and checking
// `is_cancelled` before each one. Always flushes `target` on a full,
// uncancelled completion — but flushing here only means
// `std::io::Write::flush()` (pushing the writer's own buffers out), which is
// a different guarantee from an OS-level sync/fsync of a block device to
// physical media. No write-back is requested here: see
// `write_with_writeback` for the variant the device write uses. Used by the
// development binary's PoC commands and by tests, not by the library's own
// write path any more.
#[allow(dead_code)]
pub fn write<R: Read, W: Write>(
    plan: &WritePlan,
    source: R,
    target: W,
    mut on_progress: impl FnMut(WriteProgress),
    is_cancelled: impl FnMut() -> bool,
) -> Result<u64, WriteError> {
    write_inner(
        plan,
        source,
        NoWriteback(target),
        None,
        |progress| {
            if let WriteLoopProgress::Accepted(progress) = progress {
                on_progress(progress)
            }
        },
        is_cancelled,
    )
}

// `write`, bounding how far accepted bytes run ahead of written-back ones.
// After each chunk is written:
//
//   1. accepted progress is reported (`WriteLoopProgress::Accepted`);
//   2. write-back of that chunk is started (`start_writeback`);
//   3. if more than `window` bytes are accepted but not yet confirmed, the
//      loop waits (`wait_writeback`) for the oldest of them, from the end of
//      the confirmed range up to `accepted - window`;
//   4. once that wait succeeded, the new end of the confirmed range is
//      reported (`WriteLoopProgress::WrittenBack`).
//
// So after every chunk, `accepted - written back <= window` (between steps
// 1 and 3 it can reach `window + chunk`), the written-back value only grows,
// is reported only when it grows, and never passes the accepted value. The
// last `window` bytes or less are left to the caller's final sync (or a
// cancelled write's drain), which confirms them; this loop never reports
// the total by itself unless the image fits in what it already waited for.
//
// Fallback: if the target answers a ranged request with
// `WritebackError::Unsupported`, the loop switches, for the rest of the
// write, to `sync_data()` whenever the window is exceeded -- a whole-file
// data sync that confirms everything accepted so far. A
// `WritebackError::Failed`, or a failed `sync_data()`, ends the write with
// `WriteError::Writeback`; no written-back progress is reported for it.
pub fn write_with_writeback<R: Read, W: Write + Writeback>(
    plan: &WritePlan,
    source: R,
    target: W,
    window: NonZeroU64,
    on_progress: impl FnMut(WriteLoopProgress),
    is_cancelled: impl FnMut() -> bool,
) -> Result<u64, WriteError> {
    write_inner(
        plan,
        source,
        target,
        Some(window),
        on_progress,
        is_cancelled,
    )
}

fn write_inner<R: Read, W: Write + Writeback>(
    plan: &WritePlan,
    mut source: R,
    mut target: W,
    window: Option<NonZeroU64>,
    mut on_progress: impl FnMut(WriteLoopProgress),
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<u64, WriteError> {
    // Defense in depth: even if a `WritePlan` were constructed some other
    // way than `WritePlan::new` (bypassing its validation), a zero-byte
    // image or a zero-sized chunk must never be silently accepted.
    if plan.image_size == 0 {
        return Err(WriteError::InvalidSize);
    }

    if plan.chunk_size == 0 {
        return Err(WriteError::InvalidChunkSize);
    }

    let mut buffer = vec![0u8; plan.chunk_size];
    let mut bytes_written: u64 = 0;
    let mut writeback = window.map(WritebackWindow::new);

    while bytes_written < plan.image_size {
        if is_cancelled() {
            return Err(WriteError::Cancelled { bytes_written });
        }

        let remaining = plan.image_size - bytes_written;
        // Bounded by `remaining`, so this never asks to read (and therefore
        // never writes) past `plan.image_size`, no matter how much more data
        // `source` actually has available.
        let to_read = remaining.min(buffer.len() as u64) as usize;

        let read_bytes =
            read_fully(&mut source, &mut buffer[..to_read]).map_err(WriteError::SourceRead)?;

        if read_bytes == 0 {
            // The source ran out before reaching `plan.image_size`. Reporting
            // `Ok(bytes_written)` here would silently claim success for a
            // shorter-than-planned write, so this is an explicit error
            // instead — the caller still learns exactly how much was
            // written via `bytes_written`.
            return Err(WriteError::SourceTooShort { bytes_written });
        }

        target
            .write_all(&buffer[..read_bytes])
            .map_err(WriteError::TargetWrite)?;

        let chunk_start = bytes_written;
        bytes_written += read_bytes as u64;

        on_progress(WriteLoopProgress::Accepted(WriteProgress {
            bytes_written,
            total_bytes: plan.image_size,
        }));

        if let Some(writeback) = writeback.as_mut()
            && let Some(completed_bytes) =
                writeback.after_chunk(&mut target, chunk_start, bytes_written)?
        {
            on_progress(WriteLoopProgress::WrittenBack(WritebackProgress {
                completed_bytes,
                total_bytes: plan.image_size,
            }));
        }
    }

    target.flush().map_err(WriteError::FlushFailed)?;

    Ok(bytes_written)
}

// How the write loop confirms write-back: ranged requests, or -- once the
// target said they are unsupported -- whole-file data syncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WritebackMode {
    Ranged,
    DataSync,
}

// The write loop's write-back bookkeeping: the window, the mode, and the
// end of the range confirmed so far (`completed`, from the start of the
// image; only ever set from a successful wait or sync).
struct WritebackWindow {
    window: u64,
    mode: WritebackMode,
    completed: u64,
}

impl WritebackWindow {
    fn new(window: NonZeroU64) -> Self {
        WritebackWindow {
            window: window.get(),
            mode: WritebackMode::Ranged,
            completed: 0,
        }
    }

    // After the chunk `[chunk_start, accepted)` was written: starts its
    // write-back, and if the window is exceeded, waits until it is not.
    // Returns the new end of the confirmed range when it moved.
    fn after_chunk<W: Writeback>(
        &mut self,
        target: &mut W,
        chunk_start: u64,
        accepted: u64,
    ) -> Result<Option<u64>, WriteError> {
        let failed = |error| WriteError::Writeback {
            bytes_written: accepted,
            error,
        };

        if self.mode == WritebackMode::Ranged {
            match target.start_writeback(chunk_start, accepted - chunk_start) {
                Ok(()) => {}
                Err(WritebackError::Unsupported(_)) => self.mode = WritebackMode::DataSync,
                Err(WritebackError::Failed(error)) => return Err(failed(error)),
            }
        }

        if accepted - self.completed <= self.window {
            return Ok(None);
        }

        let completed = match self.mode {
            WritebackMode::Ranged => {
                let until = accepted - self.window;
                match target.wait_writeback(self.completed, until - self.completed) {
                    Ok(()) => until,
                    Err(WritebackError::Unsupported(_)) => {
                        self.mode = WritebackMode::DataSync;
                        target.sync_data().map_err(failed)?;
                        accepted
                    }
                    Err(WritebackError::Failed(error)) => return Err(failed(error)),
                }
            }
            WritebackMode::DataSync => {
                target.sync_data().map_err(failed)?;
                accepted
            }
        };

        self.completed = completed;
        Ok(Some(completed))
    }
}

// Adapts a plain `Write` target to `write_inner` for `write`, which never
// requests write-back (it passes no window), so these are never called.
#[allow(dead_code)] // only `write` builds it; see there
struct NoWriteback<W>(W);

impl<W: Write> Write for NoWriteback<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W> Writeback for NoWriteback<W> {
    fn start_writeback(&mut self, _offset: u64, _len: u64) -> Result<(), WritebackError> {
        Ok(())
    }

    fn wait_writeback(&mut self, _offset: u64, _len: u64) -> Result<(), WritebackError> {
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Minimal verification primitive: compares two readers byte-for-byte.
// Intended to grow into distinct Quick Verify (e.g. sampled chunks or a
// hash) / Full Verify (this, exhaustive) / None policies later; today it
// only implements the exhaustive comparison.
// Used only by the CLI binary (`writer-test`); unused in the library build.
#[allow(dead_code)]
pub fn verify_equal<A: Read, B: Read>(mut a: A, mut b: B) -> io::Result<bool> {
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];

    loop {
        let n_a = read_fully(&mut a, &mut buf_a)?;
        let n_b = read_fully(&mut b, &mut buf_b)?;

        if n_a != n_b || buf_a[..n_a] != buf_b[..n_b] {
            return Ok(false);
        }

        if n_a == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated source read failure"))
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated target write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailingWriter {
        inner: Vec<u8>,
    }

    impl Write for FlushFailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("simulated flush failure"))
        }
    }

    // A. image < target -> plan created successfully.
    #[test]
    fn plan_allows_image_smaller_than_target() {
        assert!(WritePlan::new(10, 20, DEFAULT_CHUNK_SIZE).is_ok());
    }

    // B. image == target -> success.
    #[test]
    fn plan_allows_image_equal_to_target() {
        assert!(WritePlan::new(10, 10, DEFAULT_CHUNK_SIZE).is_ok());
    }

    // C. image > target -> ImageTooLarge.
    #[test]
    fn plan_rejects_image_larger_than_target() {
        assert!(matches!(
            WritePlan::new(20, 10, DEFAULT_CHUNK_SIZE),
            Err(WriteError::ImageTooLarge)
        ));
    }

    // D. image size 0 -> InvalidSize.
    #[test]
    fn plan_rejects_zero_image_size() {
        assert!(matches!(
            WritePlan::new(0, 10, DEFAULT_CHUNK_SIZE),
            Err(WriteError::InvalidSize)
        ));
    }

    // E. target size 0 -> InvalidSize.
    #[test]
    fn plan_rejects_zero_target_size() {
        assert!(matches!(
            WritePlan::new(10, 0, DEFAULT_CHUNK_SIZE),
            Err(WriteError::InvalidSize)
        ));
    }

    // F. small data -> all bytes written correctly.
    #[test]
    fn small_data_is_written_completely() {
        let data = b"hello world".to_vec();
        let plan =
            WritePlan::new(data.len() as u64, data.len() as u64, DEFAULT_CHUNK_SIZE).unwrap();
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();

        let written = write(&plan, source, &mut target, |_| {}, || false).unwrap();

        assert_eq!(written, data.len() as u64);
        assert_eq!(target, data);
    }

    // G. chunk size smaller than the data -> multiple chunks reconstruct it exactly.
    #[test]
    fn multiple_chunks_reconstruct_exact_data() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 1024).unwrap();
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();

        let written = write(&plan, source, &mut target, |_| {}, || false).unwrap();

        assert_eq!(written, data.len() as u64);
        assert_eq!(target, data);
    }

    // H. progress -> bytes_written is monotonically increasing and ends at image_size.
    #[test]
    fn progress_is_monotonic_and_ends_at_image_size() {
        let data = vec![7u8; 5000];
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 1024).unwrap();
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();
        let mut progress_log = Vec::new();

        write(
            &plan,
            source,
            &mut target,
            |progress| progress_log.push(progress.bytes_written),
            || false,
        )
        .unwrap();

        assert!(!progress_log.is_empty());
        assert!(progress_log.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(*progress_log.last().unwrap(), data.len() as u64);
    }

    // I. cancel -> stops early with Cancelled, fewer than image_size bytes written.
    #[test]
    fn cancellation_stops_before_completion() {
        let data = vec![1u8; 10_000];
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 1024).unwrap();
        let source = Cursor::new(data);
        let mut target = Vec::new();
        let mut checks = 0;

        let result = write(
            &plan,
            source,
            &mut target,
            |_| {},
            || {
                checks += 1;
                checks > 2
            },
        );

        match result {
            Err(WriteError::Cancelled { bytes_written }) => {
                assert!(bytes_written > 0);
                assert!(bytes_written < 10_000);
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    // J. source read error -> SourceRead.
    #[test]
    fn source_read_error_is_reported() {
        let plan = WritePlan::new(100, 100, 1024).unwrap();
        let mut target = Vec::new();

        let result = write(&plan, FailingReader, &mut target, |_| {}, || false);

        assert!(matches!(result, Err(WriteError::SourceRead(_))));
    }

    // K. target write error -> TargetWrite.
    #[test]
    fn target_write_error_is_reported() {
        let data = vec![1u8; 100];
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 1024).unwrap();
        let source = Cursor::new(data);

        let result = write(&plan, source, FailingWriter, |_| {}, || false);

        assert!(matches!(result, Err(WriteError::TargetWrite(_))));
    }

    // L. flush error -> FlushFailed.
    #[test]
    fn flush_error_is_reported() {
        let data = vec![1u8; 100];
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 1024).unwrap();
        let source = Cursor::new(data);
        let target = FlushFailingWriter { inner: Vec::new() };

        let result = write(&plan, source, target, |_| {}, || false);

        assert!(matches!(result, Err(WriteError::FlushFailed(_))));
    }

    // M. write then read-back matches the original exactly (via verify_equal).
    #[test]
    fn written_data_round_trips_via_verify_equal() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 250) as u8).collect();
        let plan = WritePlan::new(data.len() as u64, data.len() as u64, 512).unwrap();
        let source = Cursor::new(data.clone());
        let mut target = Vec::new();

        write(&plan, source, &mut target, |_| {}, || false).unwrap();

        let matches = verify_equal(Cursor::new(data), Cursor::new(target)).unwrap();
        assert!(matches);
    }

    // N. A zero-byte plan must never be treated as a successful write, even
    // if something bypassed `WritePlan::new`'s own validation.
    #[test]
    fn zero_byte_plan_is_never_treated_as_a_successful_write() {
        let plan = WritePlan {
            image_size: 0,
            target_size: 10,
            chunk_size: DEFAULT_CHUNK_SIZE,
        };
        let source = Cursor::new(Vec::<u8>::new());
        let mut target = Vec::new();

        let result = write(&plan, source, &mut target, |_| {}, || false);

        assert!(matches!(result, Err(WriteError::InvalidSize)));
    }

    // O. chunk_size == 0 must never be accepted by WritePlan::new, and
    // write() itself refuses it defensively too (in case a plan bypassed
    // `new`), rather than silently coercing it to some fallback chunk size.
    #[test]
    fn chunk_size_zero_is_rejected() {
        assert!(matches!(
            WritePlan::new(10, 10, 0),
            Err(WriteError::InvalidChunkSize)
        ));

        let plan = WritePlan {
            image_size: 10,
            target_size: 10,
            chunk_size: 0,
        };
        let source = Cursor::new(vec![1u8; 10]);
        let mut target = Vec::new();

        let result = write(&plan, source, &mut target, |_| {}, || false);

        assert!(matches!(result, Err(WriteError::InvalidChunkSize)));
    }

    // P. A source that runs out before `image_size` bytes are read must be
    // reported as an explicit error, never as a successful, shorter write.
    // `bytes_written` in the error still tells the caller how far it got.
    #[test]
    fn source_shorter_than_planned_image_is_error() {
        let plan = WritePlan::new(8192, 8192, 1024).unwrap();
        let short_source = Cursor::new(vec![1u8; 4096]);
        let mut target = Vec::new();

        let result = write(&plan, short_source, &mut target, |_| {}, || false);

        match result {
            Err(WriteError::SourceTooShort { bytes_written }) => {
                assert_eq!(bytes_written, 4096);
            }
            other => panic!("expected SourceTooShort, got {other:?}"),
        }
    }

    // Q. A source longer than the plan's image_size must not cause the
    // writer to write past image_size — only the first `image_size` bytes
    // ever reach the target, regardless of how much more `source` has.
    #[test]
    fn source_longer_than_plan_does_not_write_past_image_size() {
        let full_data: Vec<u8> = (0..8192u32).map(|i| (i % 256) as u8).collect();
        let plan = WritePlan::new(4096, 4096, 1024).unwrap();
        let source = Cursor::new(full_data.clone());
        let mut target = Vec::new();

        let written = write(&plan, source, &mut target, |_| {}, || false).unwrap();

        assert_eq!(written, 4096);
        assert_eq!(target.len(), 4096);
        assert_eq!(target, full_data[..4096]);
    }

    // ---------------------------------------------------------------------
    // Write-back window (`write_with_writeback`), against a fake target that
    // records every write-back request in one log together with the
    // progress reports, so their order can be checked.
    // ---------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Logged {
        Start(u64, u64),
        Wait(u64, u64),
        SyncData,
        Progress(WriteLoopProgress),
    }

    // How the fake answers write-back requests.
    #[derive(Clone, Copy)]
    enum Answer {
        Ok,
        Unsupported,
        Fail,
    }

    struct FakeTarget {
        data: Vec<u8>,
        log: std::rc::Rc<std::cell::RefCell<Vec<Logged>>>,
        start: Answer,
        wait: Answer,
        sync_fails: bool,
    }

    impl FakeTarget {
        fn new(log: &std::rc::Rc<std::cell::RefCell<Vec<Logged>>>) -> Self {
            FakeTarget {
                data: Vec::new(),
                log: log.clone(),
                start: Answer::Ok,
                wait: Answer::Ok,
                sync_fails: false,
            }
        }

        fn answer(answer: Answer, what: &str) -> Result<(), WritebackError> {
            match answer {
                Answer::Ok => Ok(()),
                Answer::Unsupported => Err(WritebackError::Unsupported(io::Error::other(format!(
                    "{what} unsupported"
                )))),
                Answer::Fail => Err(WritebackError::Failed(io::Error::other(format!(
                    "{what} failed"
                )))),
            }
        }
    }

    impl Write for FakeTarget {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.data.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Writeback for FakeTarget {
        fn start_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError> {
            self.log.borrow_mut().push(Logged::Start(offset, len));
            Self::answer(self.start, "start")
        }

        fn wait_writeback(&mut self, offset: u64, len: u64) -> Result<(), WritebackError> {
            self.log.borrow_mut().push(Logged::Wait(offset, len));
            Self::answer(self.wait, "wait")
        }

        fn sync_data(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push(Logged::SyncData);
            if self.sync_fails {
                Err(io::Error::other("sync failed"))
            } else {
                Ok(())
            }
        }
    }

    fn window(bytes: u64) -> NonZeroU64 {
        NonZeroU64::new(bytes).unwrap()
    }

    // Runs `write_with_writeback` over a fake target set up by `setup`;
    // returns the result, the log, and the target's bytes.
    fn run_windowed(
        image_size: u64,
        chunk_size: usize,
        window_bytes: u64,
        setup: impl FnOnce(&mut FakeTarget),
        cancel_after_chunks: Option<usize>,
    ) -> (Result<u64, WriteError>, Vec<Logged>, Vec<u8>) {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut target = FakeTarget::new(&log);
        setup(&mut target);
        let plan = WritePlan::new(image_size, image_size, chunk_size).unwrap();
        let source: Vec<u8> = (0..image_size).map(|i| (i % 251) as u8).collect();
        let result = write_with_writeback(
            &plan,
            Cursor::new(source),
            &mut target,
            window(window_bytes),
            |progress| log.borrow_mut().push(Logged::Progress(progress)),
            // Cancelled once `cancel_after_chunks` chunks were accepted.
            || {
                cancel_after_chunks.is_some_and(|limit| {
                    log.borrow()
                        .iter()
                        .filter(|entry| {
                            matches!(entry, Logged::Progress(WriteLoopProgress::Accepted(_)))
                        })
                        .count()
                        >= limit
                })
            },
        );
        let log = log.borrow().clone();
        (result, log, target.data)
    }

    fn written_back(log: &[Logged]) -> Vec<u64> {
        log.iter()
            .filter_map(|entry| match entry {
                Logged::Progress(WriteLoopProgress::WrittenBack(p)) => Some(p.completed_bytes),
                _ => None,
            })
            .collect()
    }

    // Checks the invariants over the whole progress sequence:
    // completed <= accepted <= total; written-back strictly increasing (no
    // duplicates); and after each chunk's write-back handling -- i.e. at
    // every following accepted report -- accepted - completed <= window.
    fn assert_window_invariants(log: &[Logged], total: u64, window_bytes: u64) {
        let mut accepted = 0u64;
        let mut completed = 0u64;
        for entry in log {
            match entry {
                Logged::Progress(WriteLoopProgress::Accepted(p)) => {
                    assert!(
                        accepted - completed <= window_bytes,
                        "backlog {} over window {window_bytes} after a chunk",
                        accepted - completed
                    );
                    assert!(p.bytes_written > accepted);
                    assert_eq!(p.total_bytes, total);
                    accepted = p.bytes_written;
                }
                Logged::Progress(WriteLoopProgress::WrittenBack(p)) => {
                    assert!(p.completed_bytes > completed, "not strictly increasing");
                    assert!(p.completed_bytes <= accepted, "written back past accepted");
                    assert_eq!(p.total_bytes, total);
                    completed = p.completed_bytes;
                }
                _ => {}
            }
            assert!(completed <= accepted && accepted <= total);
        }
        assert!(accepted - completed <= window_bytes);
    }

    // W1-W5. Over several windows with a tail: the backlog never stays over
    // the window, written-back progress only follows a successful wait
    // whose range ends exactly there, it increases strictly, and it never
    // passes the accepted value or the total. The last window is left to
    // the final sync: the loop's last written-back value is total - window.
    #[test]
    fn writeback_window_bounds_the_backlog_and_reports_only_after_waits() {
        let (total, chunk, window_bytes) = (5 * 4096 + 300, 1024usize, 4096u64);
        let (result, log, data) = run_windowed(total, chunk, window_bytes, |_| {}, None);

        assert_eq!(result.unwrap(), total);
        assert_eq!(data.len() as u64, total);
        assert_window_invariants(&log, total, window_bytes);

        for (index, entry) in log.iter().enumerate() {
            if let Logged::Progress(WriteLoopProgress::WrittenBack(p)) = entry {
                match &log[index - 1] {
                    Logged::Wait(offset, len) => assert_eq!(offset + len, p.completed_bytes),
                    other => panic!("written-back progress not right after a wait: {other:?}"),
                }
            }
        }
        // Every chunk's write-back is started, right after it is accepted.
        let starts = log
            .iter()
            .filter(|entry| matches!(entry, Logged::Start(..)))
            .count();
        assert_eq!(starts, (total as usize).div_ceil(chunk));
        // Waits cover the image from 0 without gaps or overlaps.
        let mut next = 0;
        for entry in &log {
            if let Logged::Wait(offset, len) = entry {
                assert_eq!(*offset, next);
                next = offset + len;
            }
        }
        assert_eq!(written_back(&log).last(), Some(&(total - window_bytes)));
    }

    // W2/W6. Exactly one window: nothing is waited for or reported as
    // written back -- the final sync confirms it.
    #[test]
    fn exactly_one_window_waits_for_nothing() {
        let (result, log, _) = run_windowed(4096, 1024, 4096, |_| {}, None);
        assert_eq!(result.unwrap(), 4096);
        assert!(!log.iter().any(|entry| matches!(entry, Logged::Wait(..))));
        assert!(written_back(&log).is_empty());
    }

    // W6. One byte over the window: the first byte is waited for and
    // reported, nothing more.
    #[test]
    fn one_byte_over_the_window_waits_for_that_byte() {
        let (result, log, _) = run_windowed(4097, 1024, 4096, |_| {}, None);
        assert_eq!(result.unwrap(), 4097);
        assert_eq!(
            log.iter()
                .filter(|entry| matches!(entry, Logged::Wait(..)))
                .collect::<Vec<_>>(),
            [&Logged::Wait(0, 1)]
        );
        assert_eq!(written_back(&log), [1]);
        assert_window_invariants(&log, 4097, 4096);
    }

    // W6. Chunks that do not divide the window, and a window smaller than a
    // chunk: the invariants still hold.
    #[test]
    fn uneven_chunks_and_small_windows_keep_the_invariants() {
        for (total, chunk, window_bytes) in [
            (10_000u64, 1000usize, 2500u64),
            (10_000, 3000, 1000),
            (7777, 1024, 1),
            (65_537, 4096, 16_384),
        ] {
            let (result, log, data) = run_windowed(total, chunk, window_bytes, |_| {}, None);
            assert_eq!(result.unwrap(), total);
            assert_eq!(data.len() as u64, total);
            assert_window_invariants(&log, total, window_bytes);
            assert_eq!(
                written_back(&log).last().copied().unwrap_or(0),
                total.saturating_sub(window_bytes),
                "{total}/{chunk}/{window_bytes}"
            );
        }
    }

    // W7. Images smaller than the window, or than one chunk: written as
    // before, nothing waited for. A zero-size image is still refused.
    #[test]
    fn small_images_wait_for_nothing_and_zero_is_still_refused() {
        for (total, chunk) in [(100u64, 1024usize), (3000, 1024)] {
            let (result, log, data) = run_windowed(total, chunk, 4096, |_| {}, None);
            assert_eq!(result.unwrap(), total);
            assert_eq!(data.len() as u64, total);
            assert!(written_back(&log).is_empty());
        }

        let plan = WritePlan {
            image_size: 0,
            target_size: 10,
            chunk_size: 1024,
        };
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = write_with_writeback(
            &plan,
            Cursor::new(Vec::new()),
            FakeTarget::new(&log),
            window(4096),
            |_| {},
            || false,
        );
        assert!(matches!(result, Err(WriteError::InvalidSize)));
    }

    // W8. A cancellation stops new writes with the backlog within the
    // window; the written-back value stays where the last wait put it.
    #[test]
    fn cancellation_leaves_at_most_one_window_pending() {
        let (result, log, _) = run_windowed(64 * 1024, 1024, 4096, |_| {}, Some(20));
        let accepted = match result {
            Err(WriteError::Cancelled { bytes_written }) => bytes_written,
            other => panic!("expected Cancelled, got {other:?}"),
        };
        let completed = written_back(&log).last().copied().unwrap_or(0);
        assert!(accepted > 4096);
        assert!(accepted - completed <= 4096);
        assert_window_invariants(&log, 64 * 1024, 4096);
    }

    // W10. A failed start or wait ends the write with `Writeback` and no
    // written-back progress for it.
    #[test]
    fn a_failed_start_or_wait_is_a_writeback_error() {
        let (result, log, _) = run_windowed(
            10_000,
            1000,
            2000,
            |target| target.start = Answer::Fail,
            None,
        );
        match result {
            Err(WriteError::Writeback {
                bytes_written,
                error,
            }) => {
                assert_eq!(bytes_written, 1000);
                assert_eq!(error.to_string(), "start failed");
            }
            other => panic!("expected Writeback, got {other:?}"),
        }
        assert!(written_back(&log).is_empty());

        let (result, log, _) = run_windowed(
            10_000,
            1000,
            2000,
            |target| target.wait = Answer::Fail,
            None,
        );
        match result {
            Err(WriteError::Writeback {
                bytes_written,
                error,
            }) => {
                assert_eq!(bytes_written, 3000);
                assert_eq!(error.to_string(), "wait failed");
            }
            other => panic!("expected Writeback, got {other:?}"),
        }
        assert!(written_back(&log).is_empty());
        assert!(!log.iter().any(|entry| matches!(entry, Logged::SyncData)));
    }

    // W10 fallback. Ranged write-back unsupported (on start): the loop
    // switches to a data sync each time the window is exceeded, reporting
    // everything accepted as written back; ranged requests stop.
    #[test]
    fn unsupported_start_falls_back_to_data_sync() {
        let (result, log, data) = run_windowed(
            10_000,
            1000,
            2500,
            |target| target.start = Answer::Unsupported,
            None,
        );
        assert_eq!(result.unwrap(), 10_000);
        assert_eq!(data.len(), 10_000);
        assert_eq!(
            log.iter()
                .filter(|entry| matches!(entry, Logged::Start(..)))
                .count(),
            1,
            "no ranged request after the first unsupported one"
        );
        assert!(!log.iter().any(|entry| matches!(entry, Logged::Wait(..))));
        assert_eq!(written_back(&log), [3000, 6000, 9000]);
        for (index, entry) in log.iter().enumerate() {
            if let Logged::Progress(WriteLoopProgress::WrittenBack(_)) = entry {
                assert_eq!(log[index - 1], Logged::SyncData);
            }
        }
        assert_window_invariants(&log, 10_000, 2500);
    }

    // W10 fallback. A wait answered "unsupported" is replaced by a data
    // sync on the spot, which confirms everything accepted.
    #[test]
    fn unsupported_wait_falls_back_to_data_sync() {
        let (result, log, _) = run_windowed(
            10_000,
            1000,
            2500,
            |target| target.wait = Answer::Unsupported,
            None,
        );
        assert_eq!(result.unwrap(), 10_000);
        assert_eq!(written_back(&log), [3000, 6000, 9000]);
        assert_eq!(
            log.iter()
                .filter(|entry| matches!(entry, Logged::Wait(..)))
                .count(),
            1
        );
        assert_window_invariants(&log, 10_000, 2500);
    }

    // W10 fallback. A failed fallback sync is a failure, not another
    // fallback, and reports nothing as written back.
    #[test]
    fn a_failed_fallback_sync_is_a_writeback_error() {
        let (result, log, _) = run_windowed(
            10_000,
            1000,
            2500,
            |target| {
                target.start = Answer::Unsupported;
                target.sync_fails = true;
            },
            None,
        );
        match result {
            Err(WriteError::Writeback { bytes_written, .. }) => assert_eq!(bytes_written, 3000),
            other => panic!("expected Writeback, got {other:?}"),
        }
        assert!(written_back(&log).is_empty());
    }

    // `write` (no window) never requests write-back and reports accepted
    // progress exactly as before.
    #[test]
    fn plain_write_reports_accepted_progress_only() {
        let plan = WritePlan::new(5000, 5000, 1000).unwrap();
        let mut reported = Vec::new();
        let mut target = Vec::new();
        write(
            &plan,
            Cursor::new(vec![1u8; 5000]),
            &mut target,
            |p| reported.push(p.bytes_written),
            || false,
        )
        .unwrap();
        assert_eq!(reported, [1000, 2000, 3000, 4000, 5000]);
    }
}
