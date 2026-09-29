// The GUI's presentation state and the pure rules over it: which target is
// selected across refreshes, which Verify mode is in effect, whether the
// image fits a target, and whether "Write" can be pressed. No GTK here, so
// all of it is unit tested.
//
// Nothing here is a Safety decision. Whether a device can be selected is
// the Safety Engine's (`Selectability`), whether a refreshed entry is the
// selected device is the library's identity check (`is_same_device_as`),
// and which Verify modes an image allows is the library's rule
// (`ImageInfo::verify_availability`); this module only reads those answers.
// "Write" being enabled authorizes nothing: the operation re-checks
// everything itself when it runs.

use linux_usb_writer::{DeviceCandidate, TargetRef, VerifyAvailability, VerifyMode};

// ---- Target selection ----

// What the selection rules need from a list entry. Implemented for the
// library's `DeviceCandidate` by delegating to it; tests use a stand-in that
// answers the same questions.
pub trait CandidateLike {
    type Held: Clone;

    // The Safety Engine's verdict.
    fn selectable(&self) -> bool;
    // The library's answer to "is this entry the held target's device and
    // instance?".
    fn matches(&self, held: &Self::Held) -> bool;
    // A reference to hold for this entry.
    fn held(&self) -> Self::Held;
    fn size(&self) -> u64;
}

impl CandidateLike for DeviceCandidate {
    type Held = TargetRef;

    fn selectable(&self) -> bool {
        self.selectability().is_selectable()
    }

    fn matches(&self, held: &TargetRef) -> bool {
        self.is_same_device_as(held)
    }

    fn held(&self) -> TargetRef {
        self.target().clone()
    }

    fn size(&self) -> u64 {
        self.display().size
    }
}

// What is known about the image, as far as target choice is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageContext {
    // No successfully inspected image yet (none, inspecting, or refused): a
    // target is not chosen before the image is known.
    NotReady,
    // `logical_size` is `None` for a compressed image before Preflight.
    Ready { logical_size: Option<u64> },
}

// Which group an entry is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Available,
    // Selectable, but the image is known to be larger (compatibility, not
    // Safety).
    TooSmall { shortfall: u64 },
    // The Safety Engine does not allow selecting it.
    Protected,
}

pub fn eligibility<C: CandidateLike>(candidate: &C, image: ImageContext) -> Eligibility {
    if !candidate.selectable() {
        return Eligibility::Protected;
    }
    match image {
        ImageContext::Ready { logical_size } => {
            match capacity_fit(logical_size, candidate.size()) {
                Fit::TooSmall { shortfall } => Eligibility::TooSmall { shortfall },
                Fit::Fits | Fit::UnknownUntilPreparing => Eligibility::Available,
            }
        }
        ImageContext::NotReady => Eligibility::Available,
    }
}

// How the current selection came about (presentation only; both go through
// the same confirmation later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOrigin {
    Auto,
    Manual,
}

#[derive(Debug, Clone)]
pub struct Selection<H> {
    pub held: H,
    pub origin: SelectionOrigin,
}

// Why a selection ended without the user choosing another target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearReason {
    // No entry is the selected device and instance any more.
    Disappeared,
    // Still listed, but the Safety Engine no longer allows selecting it
    // (e.g. it was mounted).
    NoLongerSelectable,
    // Still selectable, but the (new) image is known not to fit.
    TooSmall,
    // The user asked to write the same image to another drive (a result's
    // "write to another USB"): the drive just written is not kept, and none
    // is selected automatically -- it could be that same drive again.
    ChooseAnother,
}

// The target side of the GUI state.
#[derive(Debug, Clone)]
pub struct TargetChoice<H> {
    pub selection: Option<Selection<H>>,
    // Set when a selection was cleared; until the user picks a target again,
    // nothing is selected automatically -- a removed drive is never silently
    // replaced by another one.
    pub cleared: Option<ClearReason>,
}

impl<H> Default for TargetChoice<H> {
    fn default() -> Self {
        TargetChoice {
            selection: None,
            cleared: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Reconciled<H> {
    pub choice: TargetChoice<H>,
    // The index of the selected entry in the refreshed list.
    pub index: Option<usize>,
}

// Applies a refreshed list (or a newly inspected image) to the choice:
//   - a selection is kept only while an entry is the same device and
//     instance (the library's check, never a path), still selectable and not
//     known to be too small; the held reference is then the refreshed
//     entry's. A newly added device never replaces it;
//   - otherwise it is cleared, and automatic selection stays off until the
//     user picks a target;
//   - with nothing selected, automatic selection on, and an inspected image,
//     exactly one available entry is selected automatically (none, or two
//     or more, are left to the user).
pub fn reconcile<C: CandidateLike>(
    choice: TargetChoice<C::Held>,
    candidates: &[C],
    image: ImageContext,
) -> Reconciled<C::Held> {
    if let Some(selection) = choice.selection {
        let found = candidates
            .iter()
            .position(|candidate| candidate.matches(&selection.held));
        let cleared = |reason| Reconciled {
            choice: TargetChoice {
                selection: None,
                cleared: Some(reason),
            },
            index: None,
        };
        return match found {
            None => cleared(ClearReason::Disappeared),
            Some(index) => match eligibility(&candidates[index], image) {
                Eligibility::Protected => cleared(ClearReason::NoLongerSelectable),
                Eligibility::TooSmall { .. } => cleared(ClearReason::TooSmall),
                Eligibility::Available => Reconciled {
                    choice: TargetChoice {
                        selection: Some(Selection {
                            held: candidates[index].held(),
                            origin: selection.origin,
                        }),
                        cleared: None,
                    },
                    index: Some(index),
                },
            },
        };
    }

    let unchanged = Reconciled {
        choice: TargetChoice {
            selection: None,
            cleared: choice.cleared,
        },
        index: None,
    };
    if choice.cleared.is_some() || image == ImageContext::NotReady {
        return unchanged;
    }
    let mut available = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| eligibility(*candidate, image) == Eligibility::Available);
    match (available.next(), available.next()) {
        (Some((index, candidate)), None) => Reconciled {
            choice: TargetChoice {
                selection: Some(Selection {
                    held: candidate.held(),
                    origin: SelectionOrigin::Auto,
                }),
                cleared: None,
            },
            index: Some(index),
        },
        _ => unchanged,
    }
}

// The user picked an entry: only an available one, once the image is known.
pub fn select_manually<C: CandidateLike>(
    candidate: &C,
    image: ImageContext,
) -> Option<TargetChoice<C::Held>> {
    let available =
        image != ImageContext::NotReady && eligibility(candidate, image) == Eligibility::Available;
    available.then(|| TargetChoice {
        selection: Some(Selection {
            held: candidate.held(),
            origin: SelectionOrigin::Manual,
        }),
        cleared: None,
    })
}

// ---- Verify mode ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyState {
    // The mode that will be used.
    pub mode: VerifyMode,
    // The mode the user chose (or was moved to from their choice), if any;
    // kept across images.
    pub preferred: Option<VerifyMode>,
    // Set when the user's choice could not be used for the current image.
    pub notice: Option<VerifyNotice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyNotice {
    pub wanted: VerifyMode,
    pub used: VerifyMode,
}

impl VerifyState {
    // Before any image: shown, not yet settable.
    pub fn initial() -> Self {
        VerifyState {
            mode: VerifyMode::Quick,
            preferred: None,
            notice: None,
        }
    }

    // A new image was inspected. Without an explicit choice the recommended
    // policy applies (Quick when available, else Full). An explicit choice
    // is kept when the image allows it; otherwise Full is used, the notice
    // says so, and Full becomes the choice -- the earlier mode is not brought
    // back by a later image.
    pub fn for_image(self, available: impl Fn(VerifyMode) -> VerifyAvailability) -> Self {
        let usable = |mode| available(mode) == VerifyAvailability::Available;
        let first_usable = |modes: &[VerifyMode]| {
            modes
                .iter()
                .copied()
                .find(|&mode| usable(mode))
                .unwrap_or(VerifyMode::None)
        };
        match self.preferred {
            None => VerifyState {
                mode: first_usable(&[VerifyMode::Quick, VerifyMode::Full]),
                preferred: None,
                notice: None,
            },
            Some(wanted) if usable(wanted) => VerifyState {
                mode: wanted,
                preferred: Some(wanted),
                notice: None,
            },
            Some(wanted) => {
                let used = first_usable(&[VerifyMode::Full]);
                VerifyState {
                    mode: used,
                    preferred: Some(used),
                    notice: Some(VerifyNotice { wanted, used }),
                }
            }
        }
    }

    // The user chose `mode` (only offered when available).
    pub fn chosen(mode: VerifyMode) -> Self {
        VerifyState {
            mode,
            preferred: Some(mode),
            notice: None,
        }
    }

    // The image was let go: only the user's preference is kept. The mode in
    // effect and the notice belonged to that image; the next image starts
    // from the preference (`for_image`).
    pub fn without_image(self) -> Self {
        VerifyState {
            preferred: self.preferred,
            ..VerifyState::initial()
        }
    }
}

// ---- Leaving a result ----

// What the main view keeps when the user leaves a finished operation's
// result. The Verify preference is always kept. Nothing of the operation
// itself is kept (the window drops it, Safe Removal's target with it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainReturn {
    pub keep_image: bool,
    pub target: TargetReturn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetReturn {
    // The selection stays as it is; the next refresh applies the usual
    // rules to it (`reconcile`: same device and instance, still selectable,
    // still large enough).
    Keep,
    // Let go, and none selected automatically (`ClearReason::ChooseAnother`).
    ChooseAnother,
    // Back to the start: nothing selected, automatic selection as at start.
    Reset,
}

// The target choice and Verify state after leaving a result.
pub fn return_to_main<H>(
    ret: MainReturn,
    choice: TargetChoice<H>,
    verify: VerifyState,
) -> (TargetChoice<H>, VerifyState) {
    let choice = match ret.target {
        TargetReturn::Keep => choice,
        TargetReturn::ChooseAnother => TargetChoice {
            selection: None,
            cleared: Some(ClearReason::ChooseAnother),
        },
        TargetReturn::Reset => TargetChoice::default(),
    };
    let verify = if ret.keep_image {
        verify
    } else {
        verify.without_image()
    };
    (choice, verify)
}

// ---- Capacity (compatibility, not Safety) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Fits,
    TooSmall { shortfall: u64 },
    // A compressed image's size is only known once the operation has
    // validated it; the operation checks the capacity then.
    UnknownUntilPreparing,
}

pub fn capacity_fit(logical_size: Option<u64>, target_size: u64) -> Fit {
    match logical_size {
        None => Fit::UnknownUntilPreparing,
        Some(size) if size > target_size => Fit::TooSmall {
            shortfall: size - target_size,
        },
        Some(_) => Fit::Fits,
    }
}

// ---- Whether "Write" can be pressed ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePhase {
    Missing,
    Inspecting,
    Invalid,
    Ready { logical_size: Option<u64> },
}

impl ImagePhase {
    pub fn context(self) -> ImageContext {
        match self {
            ImagePhase::Ready { logical_size } => ImageContext::Ready { logical_size },
            ImagePhase::Missing | ImagePhase::Inspecting | ImagePhase::Invalid => {
                ImageContext::NotReady
            }
        }
    }
}

// Why "Write" is disabled, first reason first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBlocker {
    NoImage,
    ImageInspecting,
    ImageInvalid,
    // The latest device list could not be read, so the selected target is
    // not known to be present.
    DeviceListUnavailable,
    NoTarget,
    VerifyUnavailable,
    TooSmall { shortfall: u64 },
}

// `target_size` is the selected target's size (a selection is always a
// selectable entry).
pub fn write_readiness(
    image: ImagePhase,
    device_list_ok: bool,
    target_size: Option<u64>,
    verify_available: bool,
) -> Result<(), WriteBlocker> {
    let logical_size = match image {
        ImagePhase::Missing => return Err(WriteBlocker::NoImage),
        ImagePhase::Inspecting => return Err(WriteBlocker::ImageInspecting),
        ImagePhase::Invalid => return Err(WriteBlocker::ImageInvalid),
        ImagePhase::Ready { logical_size } => logical_size,
    };
    if !device_list_ok {
        return Err(WriteBlocker::DeviceListUnavailable);
    }
    let Some(target_size) = target_size else {
        return Err(WriteBlocker::NoTarget);
    };
    if !verify_available {
        return Err(WriteBlocker::VerifyUnavailable);
    }
    match capacity_fit(logical_size, target_size) {
        Fit::TooSmall { shortfall } => Err(WriteBlocker::TooSmall { shortfall }),
        Fit::Fits | Fit::UnknownUntilPreparing => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_usb_writer::{ImageInfo, inspect_image};

    // A list entry whose identity answers stand in for the library's: the
    // same `device` and `instance` means "same device and instance" -- the
    // path plays no part, as in `is_same_device_as`.
    #[derive(Debug, Clone)]
    struct Entry {
        path: &'static str,
        device: u32,
        instance: u32,
        selectable: bool,
        size: u64,
    }

    impl CandidateLike for Entry {
        type Held = Entry;

        fn selectable(&self) -> bool {
            self.selectable
        }

        fn matches(&self, held: &Entry) -> bool {
            self.device == held.device && self.instance == held.instance
        }

        fn held(&self) -> Entry {
            self.clone()
        }

        fn size(&self) -> u64 {
            self.size
        }
    }

    const GB: u64 = 1_000_000_000;

    fn usb(path: &'static str, device: u32) -> Entry {
        Entry {
            path,
            device,
            instance: 1,
            selectable: true,
            size: 8 * GB,
        }
    }

    fn internal() -> Entry {
        Entry {
            path: "/dev/nvme0n1",
            device: 99,
            instance: 1,
            selectable: false,
            size: 256 * GB,
        }
    }

    const RAW_2GB: ImageContext = ImageContext::Ready {
        logical_size: Some(2 * GB),
    };
    const COMPRESSED: ImageContext = ImageContext::Ready { logical_size: None };

    fn path(reconciled: &Reconciled<Entry>) -> Option<&'static str> {
        reconciled.choice.selection.as_ref().map(|s| s.held.path)
    }

    fn fresh() -> TargetChoice<Entry> {
        TargetChoice::default()
    }

    #[test]
    fn nothing_is_selected_before_the_image_is_known() {
        let listed = reconcile(fresh(), &[usb("/dev/sda", 1)], ImageContext::NotReady);
        assert_eq!(path(&listed), None);
        assert!(select_manually(&usb("/dev/sda", 1), ImageContext::NotReady).is_none());
    }

    #[test]
    fn exactly_one_available_target_is_selected_automatically() {
        let none = reconcile(fresh(), &[internal()], RAW_2GB);
        assert_eq!(path(&none), None);

        let one = reconcile(fresh(), &[internal(), usb("/dev/sda", 1)], RAW_2GB);
        assert_eq!(path(&one), Some("/dev/sda"));
        assert_eq!(one.index, Some(1));
        assert_eq!(one.choice.selection.unwrap().origin, SelectionOrigin::Auto);

        let two = reconcile(fresh(), &[usb("/dev/sda", 1), usb("/dev/sdb", 2)], RAW_2GB);
        assert_eq!(path(&two), None);

        // A compressed image's unknown size never counts against a target.
        let compressed = reconcile(fresh(), &[usb("/dev/sda", 1)], COMPRESSED);
        assert_eq!(path(&compressed), Some("/dev/sda"));
    }

    #[test]
    fn a_target_too_small_for_the_image_is_neither_chosen_nor_kept() {
        let mut small = usb("/dev/sdb", 2);
        small.size = GB;
        assert_eq!(
            eligibility(&small, RAW_2GB),
            Eligibility::TooSmall { shortfall: GB }
        );
        assert_eq!(eligibility(&small, COMPRESSED), Eligibility::Available);
        assert_eq!(eligibility(&internal(), RAW_2GB), Eligibility::Protected);

        // One that fits and one that does not: the fitting one is the only
        // available one.
        let chosen = reconcile(fresh(), &[small.clone(), usb("/dev/sda", 1)], RAW_2GB);
        assert_eq!(path(&chosen), Some("/dev/sda"));
        assert!(select_manually(&small, RAW_2GB).is_none());

        // A selection the new image does not fit is cleared.
        let held = select_manually(&small, COMPRESSED).unwrap();
        let refreshed = reconcile(held, &[small], RAW_2GB);
        assert_eq!(path(&refreshed), None);
        assert_eq!(refreshed.choice.cleared, Some(ClearReason::TooSmall));
    }

    #[test]
    fn a_selection_is_kept_when_another_drive_appears() {
        let manual = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let refreshed = reconcile(manual, &[usb("/dev/sdb", 2), usb("/dev/sda", 1)], RAW_2GB);
        assert_eq!(path(&refreshed), Some("/dev/sda"));
        assert_eq!(refreshed.index, Some(1));
        assert_eq!(
            refreshed.choice.selection.unwrap().origin,
            SelectionOrigin::Manual
        );

        // An automatic selection is not replaced either.
        let auto = reconcile(fresh(), &[usb("/dev/sda", 1)], RAW_2GB).choice;
        let refreshed = reconcile(auto, &[usb("/dev/sda", 1), usb("/dev/sdb", 2)], RAW_2GB);
        assert_eq!(path(&refreshed), Some("/dev/sda"));
        assert_eq!(
            refreshed.choice.selection.unwrap().origin,
            SelectionOrigin::Auto
        );

        // A new image being inspected does not drop the selection.
        let manual = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let inspecting = reconcile(manual, &[usb("/dev/sda", 1)], ImageContext::NotReady);
        assert_eq!(path(&inspecting), Some("/dev/sda"));
    }

    #[test]
    fn a_selection_ends_when_its_device_or_instance_is_gone() {
        let held = || select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();

        let gone = reconcile(held(), &[usb("/dev/sdb", 2)], RAW_2GB);
        assert_eq!(path(&gone), None);
        assert_eq!(gone.choice.cleared, Some(ClearReason::Disappeared));

        // Same path, new instance (re-plugged): not the selected device.
        let mut replugged = usb("/dev/sda", 1);
        replugged.instance = 2;
        let replaced = reconcile(held(), &[replugged], RAW_2GB);
        assert_eq!(path(&replaced), None);
        assert_eq!(replaced.choice.cleared, Some(ClearReason::Disappeared));

        // Same path, another device.
        assert_eq!(
            path(&reconcile(held(), &[usb("/dev/sda", 7)], RAW_2GB)),
            None
        );

        // Still there but no longer selectable (e.g. mounted).
        let mut mounted = usb("/dev/sda", 1);
        mounted.selectable = false;
        let refused = reconcile(held(), &[mounted], RAW_2GB);
        assert_eq!(
            refused.choice.cleared,
            Some(ClearReason::NoLongerSelectable)
        );
    }

    #[test]
    fn no_automatic_selection_after_a_selection_was_cleared() {
        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let only_other = [usb("/dev/sdb", 2)];

        let cleared = reconcile(held, &only_other, RAW_2GB);
        assert_eq!(path(&cleared), None);
        // Later refreshes, and a new image, still leave it to the user...
        let later = reconcile(cleared.choice, &only_other, RAW_2GB);
        assert_eq!(path(&later), None);
        let later = reconcile(later.choice, &only_other, COMPRESSED);
        assert_eq!(path(&later), None);
        assert_eq!(later.choice.cleared, Some(ClearReason::Disappeared));
        // ...until the user picks one.
        let picked = select_manually(&only_other[0], COMPRESSED).unwrap();
        let kept = reconcile(picked, &only_other, COMPRESSED);
        assert_eq!(path(&kept), Some("/dev/sdb"));
        assert_eq!(kept.choice.cleared, None);
    }

    #[test]
    fn a_protected_device_cannot_be_picked() {
        assert!(select_manually(&internal(), RAW_2GB).is_none());
    }

    // A uniquely named file per call: tests run in parallel.
    fn image(tag: &str, contents: &[u8]) -> ImageInfo {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "linux-usb-writer-gui-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        let info = inspect_image(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        info
    }

    fn raw() -> ImageInfo {
        image("raw.img", &[0u8; 4096])
    }

    fn gzip() -> ImageInfo {
        image("image.img.gz", &[0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00])
    }

    fn after(state: VerifyState, info: &ImageInfo) -> VerifyState {
        state.for_image(|mode| info.verify_availability(mode))
    }

    #[test]
    fn the_recommended_verify_mode_follows_the_image() {
        let state = after(VerifyState::initial(), &raw());
        assert_eq!(state.mode, VerifyMode::Quick);
        assert_eq!(state.notice, None);

        let state = after(state, &gzip());
        assert_eq!(state.mode, VerifyMode::Full);
        assert_eq!(state.notice, None);

        let state = after(state, &raw());
        assert_eq!(state.mode, VerifyMode::Quick);
    }

    #[test]
    fn an_explicit_verify_choice_is_kept_while_the_image_allows_it() {
        let full = after(VerifyState::chosen(VerifyMode::Full), &raw());
        assert_eq!(full.mode, VerifyMode::Full);
        let full = after(full, &gzip());
        assert_eq!(full.mode, VerifyMode::Full);
        assert_eq!(full.notice, None);

        let none = after(VerifyState::chosen(VerifyMode::None), &gzip());
        assert_eq!(none.mode, VerifyMode::None);
        assert_eq!(after(none, &raw()).mode, VerifyMode::None);

        let quick = after(VerifyState::chosen(VerifyMode::Quick), &raw());
        assert_eq!(quick.mode, VerifyMode::Quick);
    }

    #[test]
    fn an_unavailable_choice_falls_back_to_full_and_stays_there() {
        let state = after(VerifyState::chosen(VerifyMode::Quick), &gzip());
        assert_eq!(state.mode, VerifyMode::Full);
        assert_eq!(
            state.notice,
            Some(VerifyNotice {
                wanted: VerifyMode::Quick,
                used: VerifyMode::Full,
            })
        );
        // The fallback is not undone by a later image that allows Quick.
        let state = after(state, &raw());
        assert_eq!(state.mode, VerifyMode::Full);
        assert_eq!(state.notice, None);
    }

    #[test]
    fn capacity_is_judged_only_when_the_size_is_known() {
        assert_eq!(capacity_fit(Some(100), 100), Fit::Fits);
        assert_eq!(capacity_fit(Some(101), 100), Fit::TooSmall { shortfall: 1 });
        assert_eq!(capacity_fit(None, 1), Fit::UnknownUntilPreparing);
    }

    #[test]
    fn write_is_enabled_only_when_everything_is_ready() {
        let ready = ImagePhase::Ready {
            logical_size: Some(100),
        };
        assert_eq!(write_readiness(ready, true, Some(1_000), true), Ok(()));
        assert_eq!(
            write_readiness(ImagePhase::Missing, true, Some(1_000), true),
            Err(WriteBlocker::NoImage)
        );
        assert_eq!(
            write_readiness(ImagePhase::Inspecting, true, Some(1_000), true),
            Err(WriteBlocker::ImageInspecting)
        );
        assert_eq!(
            write_readiness(ImagePhase::Invalid, true, Some(1_000), true),
            Err(WriteBlocker::ImageInvalid)
        );
        assert_eq!(
            write_readiness(ready, true, None, true),
            Err(WriteBlocker::NoTarget)
        );
        assert_eq!(
            write_readiness(ready, true, Some(1_000), false),
            Err(WriteBlocker::VerifyUnavailable)
        );
        assert_eq!(
            write_readiness(ready, true, Some(60), true),
            Err(WriteBlocker::TooSmall { shortfall: 40 })
        );
        // An unknown (compressed) size never disables Write by itself.
        let compressed = ImagePhase::Ready { logical_size: None };
        assert_eq!(write_readiness(compressed, true, Some(1), true), Ok(()));
        assert_eq!(
            write_readiness(ready, false, Some(1_000), true),
            Err(WriteBlocker::DeviceListUnavailable)
        );
        // The first missing piece is reported first.
        assert_eq!(
            write_readiness(ImagePhase::Missing, true, None, false),
            Err(WriteBlocker::NoImage)
        );
    }

    // ---- Leaving a result ----

    fn kept(target: TargetReturn) -> MainReturn {
        MainReturn {
            keep_image: true,
            target,
        }
    }

    fn done() -> MainReturn {
        MainReturn {
            keep_image: false,
            target: TargetReturn::Reset,
        }
    }

    #[test]
    fn keeping_the_target_keeps_the_selection_and_the_verify_state() {
        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let verify = after(VerifyState::chosen(VerifyMode::Full), &raw());
        let (choice, after_return) = return_to_main(kept(TargetReturn::Keep), held, verify);
        assert_eq!(
            choice
                .selection
                .as_ref()
                .map(|selection| selection.held.path),
            Some("/dev/sda")
        );
        assert_eq!(choice.cleared, None);
        assert_eq!(after_return, verify);
    }

    // A kept target is only shown as chosen: the next refresh judges it as
    // any selection (same device and instance, still selectable), and a
    // re-plugged or changed drive is not kept.
    #[test]
    fn a_kept_target_still_goes_through_the_refresh_rules() {
        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let (choice, _) = return_to_main(kept(TargetReturn::Keep), held, VerifyState::initial());
        let mut replugged = usb("/dev/sda", 1);
        replugged.instance = 2;
        let refreshed = reconcile(choice, &[replugged], RAW_2GB);
        assert_eq!(path(&refreshed), None);
        assert_eq!(refreshed.choice.cleared, Some(ClearReason::Disappeared));

        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let (choice, _) = return_to_main(kept(TargetReturn::Keep), held, VerifyState::initial());
        let mut mounted = usb("/dev/sda", 1);
        mounted.selectable = false;
        let refreshed = reconcile(choice, &[mounted], RAW_2GB);
        assert_eq!(path(&refreshed), None);
    }

    // "Write to another USB": the drive just written is let go and not
    // selected again automatically -- not even when it is the only one
    // listed -- until the user picks one. The image's Verify state stays.
    #[test]
    fn another_usb_lets_the_target_go_and_keeps_the_image_and_verify() {
        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let verify = after(VerifyState::chosen(VerifyMode::Full), &raw());
        let (choice, after_return) =
            return_to_main(kept(TargetReturn::ChooseAnother), held, verify);
        assert!(choice.selection.is_none());
        assert_eq!(choice.cleared, Some(ClearReason::ChooseAnother));
        assert_eq!(after_return, verify);
        // Still valid for the kept image.
        assert_eq!(
            raw().verify_availability(after_return.mode),
            VerifyAvailability::Available
        );

        let same_drive = [usb("/dev/sda", 1)];
        let refreshed = reconcile(choice, &same_drive, RAW_2GB);
        assert_eq!(path(&refreshed), None);
        let picked = select_manually(&usb("/dev/sdb", 2), RAW_2GB).unwrap();
        let refreshed = reconcile(picked, &[usb("/dev/sdb", 2)], RAW_2GB);
        assert_eq!(path(&refreshed), Some("/dev/sdb"));
    }

    // "Done": nothing selected and automatic selection as at start; only
    // the Verify preference is kept -- not the mode or notice the previous
    // image led to.
    #[test]
    fn done_starts_over_keeping_only_the_verify_preference() {
        let held = select_manually(&usb("/dev/sda", 1), RAW_2GB).unwrap();
        let verify = after(VerifyState::chosen(VerifyMode::Full), &raw());
        let (choice, after_return) = return_to_main(done(), held, verify);
        assert!(choice.selection.is_none());
        assert_eq!(choice.cleared, None);
        assert_eq!(after_return.preferred, Some(VerifyMode::Full));
        assert_eq!(after_return.notice, None);
        assert_eq!(
            after_return,
            VerifyState {
                preferred: Some(VerifyMode::Full),
                ..VerifyState::initial()
            }
        );
        // The next image applies the preference afresh.
        assert_eq!(after(after_return, &raw()).mode, VerifyMode::Full);

        // A fallback notice belonged to the previous image: gone.
        let fell_back = after(VerifyState::chosen(VerifyMode::Quick), &gzip());
        assert!(fell_back.notice.is_some());
        let (_, after_return) = return_to_main(done(), TargetChoice::<Entry>::default(), fell_back);
        assert_eq!(after_return.notice, None);
        assert_eq!(after_return.preferred, fell_back.preferred);
        let next = after(after_return, &raw());
        assert_eq!(next.notice, None);
        assert_eq!(next.mode, fell_back.preferred.unwrap());

        // Without a preference, the recommended mode applies again.
        let (_, after_return) = return_to_main(
            done(),
            TargetChoice::<Entry>::default(),
            after(VerifyState::initial(), &gzip()),
        );
        assert_eq!(after_return, VerifyState::initial());
        assert_eq!(after(after_return, &raw()).mode, VerifyMode::Quick);

        // With an image again, the only available drive is selected as at
        // start.
        let started = reconcile(choice, &[usb("/dev/sdb", 2)], RAW_2GB);
        assert_eq!(path(&started), Some("/dev/sdb"));
    }
}
