// What the GUI says, in words: the library's typed answers turned into
// short Japanese text for the normal view, and their exact names for the
// technical details. Presentation only -- every decision is the library's.

use std::io;

use linux_usb_writer::report::{CompressionFormat, ImageSourceError};
use linux_usb_writer::{
    DeviceDisplay, ImageAccess, OpenPurpose, RiskLevel, RiskReason, VerifyMode,
    VerifyUnavailableReason, WorkerConfirmationRequest,
};

use crate::model::{ClearReason, VerifyNotice, WriteBlocker};
use crate::operation::{Activity, Mark, Reason, Step, Tracker};
use crate::result::{ResultAction, ResultCase, ResultKind, StepLine};

// ---- Sizes ----

// A size as people read it (SI units, one decimal place).
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} バイト");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

// Progress as "done / total", both in the total's unit with two decimals
// (so the done part moves visibly).
pub fn transfer(done: u64, total: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if total < 1000 {
        return format!("{done} / {total} バイト");
    }
    let mut divisor = 1000.0;
    let mut unit = 0;
    while total as f64 / divisor >= 1000.0 && unit < UNITS.len() - 1 {
        divisor *= 1000.0;
        unit += 1;
    }
    format!(
        "{:.2} {unit_name} / {:.2} {unit_name}",
        done as f64 / divisor,
        total as f64 / divisor,
        unit_name = UNITS[unit]
    )
}

// The exact byte count, for the technical details.
pub fn exact_bytes(bytes: u64) -> String {
    let digits = bytes.to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} バイト")
}

// ---- Image ----

pub fn image_kind(compression: Option<CompressionFormat>) -> &'static str {
    match compression {
        None => "非圧縮イメージ",
        Some(CompressionFormat::Gzip) => "GZIP 圧縮イメージ",
        Some(CompressionFormat::Xz) => "XZ 圧縮イメージ",
    }
}

pub fn compression_name(compression: Option<CompressionFormat>) -> &'static str {
    match compression {
        None => "None",
        Some(CompressionFormat::Gzip) => "gzip",
        Some(CompressionFormat::Xz) => "xz",
    }
}

pub fn access_name(access: ImageAccess) -> &'static str {
    match access {
        ImageAccess::RandomAccess => "Random Access",
        ImageAccess::SequentialReplay => "Sequential Replay",
    }
}

// Why an image cannot be written, in the user's terms.
pub fn image_error(error: &ImageSourceError) -> String {
    match error {
        ImageSourceError::Io(error) => match error.kind() {
            io::ErrorKind::NotFound => "ファイルが見つかりません".to_string(),
            io::ErrorKind::PermissionDenied => "ファイルを読み取る権限がありません".to_string(),
            _ => "ファイルを読み取れませんでした".to_string(),
        },
        ImageSourceError::NotRegularFile => {
            "通常のファイルではありません（フォルダーやデバイスは選べません）".to_string()
        }
        ImageSourceError::UnsupportedFormat(kind) => format!(
            "{} 形式の圧縮ファイルには対応していません",
            kind.name().to_uppercase()
        ),
        ImageSourceError::ExtensionMismatch { expected } => format!(
            "ファイル名は {} 圧縮を示していますが、内容が一致しません",
            expected.name().to_uppercase()
        ),
    }
}

// ---- Targets ----

pub fn bus(connection_bus: &str) -> String {
    match connection_bus {
        "" => "接続方式不明".to_string(),
        "usb" => "USB".to_string(),
        other => other.to_uppercase(),
    }
}

// A device's name: vendor and model, or a fallback.
pub fn device_name(vendor: &str, model: &str) -> String {
    let name = [vendor.trim(), model.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        "名前のないデバイス".to_string()
    } else {
        name
    }
}

pub fn risk_reason(reason: &RiskReason) -> &'static str {
    match reason {
        RiskReason::SystemDevice => "システムのディスクです",
        RiskReason::CriticalMount => "システム領域（/ や /boot など）が使用しています",
        RiskReason::ActiveSwap => "スワップとして使用中です",
        RiskReason::ComplexStorage => "LVM・RAID・暗号化などの構成に含まれています",
        RiskReason::MountedFilesystem => "マウントされています",
        RiskReason::ReadOnly => "読み取り専用です",
        RiskReason::IgnoredBySystem => "システムが非表示に設定しています",
        RiskReason::NotPartitionable => "パーティションを作成できないデバイスです",
        RiskReason::UsbRemovable => "USB のリムーバブルデバイスです",
        RiskReason::UnknownOrNonRemovable => "USB のリムーバブルデバイスと確認できません",
        RiskReason::MediaUnavailable => "メディアが挿入されていません",
    }
}

// Why a protected device cannot be chosen: the Safety Engine's reasons as
// sentences, except the informational one a selectable device also carries.
pub fn protection(reasons: &[RiskReason]) -> String {
    let explained: Vec<&str> = reasons
        .iter()
        .filter(|reason| !matches!(reason, RiskReason::UsbRemovable))
        .map(risk_reason)
        .collect();
    if explained.is_empty() {
        "安全のため選択できません。".to_string()
    } else {
        explained
            .iter()
            .map(|sentence| format!("{sentence}。"))
            .collect()
    }
}

pub fn risk_level_name(level: &RiskLevel) -> &'static str {
    match level {
        RiskLevel::Normal => "Normal",
        RiskLevel::Caution => "Caution",
        RiskLevel::Blocked => "Blocked",
    }
}

// The detail (`CandidateListError`) goes to the technical details.
pub fn candidate_list_error_message() -> &'static str {
    "デバイスの一覧を取得できませんでした"
}

// Why the selection was cleared, when it needs saying. Choosing another
// drive was the user's own request: the empty selection says it all.
pub fn clear_reason(reason: ClearReason) -> Option<&'static str> {
    match reason {
        ClearReason::Disappeared => Some(
            "選択していた USB ドライブが見つからなくなりました。書き込み先を選び直してください。",
        ),
        ClearReason::NoLongerSelectable => {
            Some("選択していた USB ドライブは現在選択できません。書き込み先を選び直してください。")
        }
        ClearReason::TooSmall => Some(
            "選択していた USB ドライブはこのイメージには容量が足りません。書き込み先を選び直してください。",
        ),
        ClearReason::ChooseAnother => None,
    }
}

// ---- Verify ----

pub fn verify_title(mode: VerifyMode) -> &'static str {
    match mode {
        VerifyMode::Quick => "クイック検証",
        VerifyMode::Full => "完全検証",
        VerifyMode::None => "検証なし",
    }
}

pub fn verify_description(mode: VerifyMode) -> &'static str {
    match mode {
        VerifyMode::Quick => "書き込み後、一部を読み戻して確認します。",
        VerifyMode::Full => {
            "書き込んだデータをすべて読み戻して確認します。\nクイック検証より時間がかかります。"
        }
        VerifyMode::None => "書き込み後の読み戻し確認は行われません。",
    }
}

// The one explanation shown under the Verify choices: the selected mode's,
// once an image is known.
pub fn verify_help(image_ready: bool, selected: VerifyMode) -> Option<&'static str> {
    image_ready.then(|| verify_description(selected))
}

// Next to a mode the image does not allow.
pub fn verify_unavailable_short(reason: VerifyUnavailableReason) -> &'static str {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => "このイメージでは利用できません",
    }
}

// Why (tooltip and accessible description of that mode).
pub fn verify_unavailable(reason: VerifyUnavailableReason) -> &'static str {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => {
            "この形式のイメージは任意の位置から読み取れないため、一部を読み戻すクイック検証は利用できません"
        }
    }
}

// What "available" means (technical details): a format check, not a
// promise that Verify will succeed on the device.
pub fn verify_availability_note() -> &'static str {
    "イメージの形式による判定です。検証そのものは、実行時にデバイスの状態によって失敗することがあります"
}

pub fn verify_unavailable_name(reason: VerifyUnavailableReason) -> &'static str {
    match reason {
        VerifyUnavailableReason::NeedsRandomAccess => "Needs Random Access",
    }
}

pub fn verify_notice(notice: VerifyNotice) -> String {
    format!(
        "このイメージでは{}を利用できないため、{}に変更しました",
        verify_title(notice.wanted),
        verify_title(notice.used)
    )
}

// ---- Write ----

pub fn write_blocker(blocker: WriteBlocker) -> String {
    match blocker {
        WriteBlocker::NoImage => "書き込むイメージを選択してください".to_string(),
        WriteBlocker::ImageInspecting => "イメージを確認しています…".to_string(),
        WriteBlocker::ImageInvalid => "このイメージは書き込めません".to_string(),
        WriteBlocker::DeviceListUnavailable => {
            "デバイスの一覧を取得できないため、書き込み先を確認できません".to_string()
        }
        WriteBlocker::NoTarget => "書き込み先の USB ドライブを選択してください".to_string(),
        WriteBlocker::VerifyUnavailable => "この検証方法は利用できません".to_string(),
        WriteBlocker::TooSmall { shortfall } => {
            format!("容量が {} 不足しています", size(shortfall))
        }
    }
}

// ---- The operation ----

pub fn step_name(step: Step) -> &'static str {
    match step {
        Step::Prepare => "準備",
        Step::Write => "書き込み",
        Step::Verify => "検証",
    }
}

pub fn mark_label(mark: Mark) -> &'static str {
    match mark {
        Mark::Waiting => "待機中",
        Mark::Active => "実行中",
        Mark::Done => "完了",
        Mark::Skipped => "検証なし",
        Mark::Cancelled => "中止",
        Mark::Failed => "失敗",
    }
}

pub fn mark_icon(mark: Mark) -> &'static str {
    match mark {
        Mark::Waiting => "content-loading-symbolic",
        Mark::Active => "media-playback-start-symbolic",
        Mark::Done => "emblem-ok-symbolic",
        // "Skip": neutral in Adwaita and Breeze alike. Breeze draws
        // `list-remove-symbolic` as a red cross, which reads as a failure.
        Mark::Skipped => "media-skip-forward-symbolic",
        Mark::Cancelled => "process-stop-symbolic",
        Mark::Failed => "dialog-error-symbolic",
    }
}

// The operation view's heading while it runs.
pub fn headline(step: Step) -> &'static str {
    match step {
        Step::Prepare => "書き込みを準備しています",
        Step::Write => "USB に書き込んでいます",
        Step::Verify => "書き込んだデータを確認しています",
    }
}

// What the operation is doing, and a line under it.
pub fn activity(tracker: &Tracker) -> (&'static str, &'static str) {
    if tracker.cancel_requested {
        return ("中止しています…", "処理が止まるまでお待ちください。");
    }
    const NOT_WRITTEN: &str = "まだ USB ドライブには書き込んでいません。";
    const KEEP_CONNECTED: &str = "USB ドライブを取り外さないでください。";
    const AUTHENTICATION: &str = "システムの認証画面が表示された場合は、認証を完了してください。";
    match tracker.activity {
        Activity::Starting => ("書き込み先を確認しています…", NOT_WRITTEN),
        Activity::PreparingImage => ("イメージを確認しています…", NOT_WRITTEN),
        Activity::Preflight => ("圧縮イメージを確認しています…", NOT_WRITTEN),
        Activity::AwaitingConfirmation => ("最終確認を待っています…", NOT_WRITTEN),
        Activity::StartingWrite => ("書き込みを開始しています…", KEEP_CONNECTED),
        Activity::OpeningDevice(OpenPurpose::Write) => {
            ("USB ドライブを開いています…", AUTHENTICATION)
        }
        Activity::OpeningDevice(OpenPurpose::Verify) => {
            ("検証のために USB ドライブを開いています…", AUTHENTICATION)
        }
        Activity::Writing if tracker.compression.is_some() => {
            ("展開しながら書き込み中", KEEP_CONNECTED)
        }
        Activity::Writing => ("書き込み中", KEEP_CONNECTED),
        Activity::Syncing => (
            "書き込みを仕上げています…",
            "USB ドライブへの書き込みを完了しています。取り外さないでください。",
        ),
        Activity::PreparingVerify => ("検証の準備をしています…", KEEP_CONNECTED),
        Activity::Verifying => match tracker.verify_mode {
            VerifyMode::Full => ("完全検証中", "書き込んだデータをすべて確認しています。"),
            _ => ("クイック検証中", "書き込んだデータを確認しています。"),
        },
        Activity::Finished => ("", ""),
    }
}

// ---- The result ----

pub fn result_title(case: ResultCase) -> &'static str {
    match case {
        ResultCase::Verified(_) => "完了しました",
        ResultCase::WrittenWithoutVerify => "書き込みが完了しました",
        ResultCase::VerifyCancelled => "書き込みは完了しています",
        ResultCase::VerifyMismatch => "検証で問題が見つかりました",
        ResultCase::VerifyNotCompleted(_) => "検証を完了できませんでした",
        ResultCase::WriteFailed { .. } => "書き込みを完了できませんでした",
        ResultCase::WriteCancelled { .. } | ResultCase::CancelledBeforeWrite => {
            "書き込みを中止しました"
        }
        ResultCase::NotStarted(_) => "書き込みを開始できませんでした",
        ResultCase::Lost => "書き込み処理を完了できませんでした",
    }
}

pub fn result_icon(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::Success => "emblem-ok-symbolic",
        ResultKind::WrittenNotVerified => "dialog-information-symbolic",
        ResultKind::Cancelled => "process-stop-symbolic",
        ResultKind::Failed => "dialog-warning-symbolic",
    }
}

// What one step says on the result view: the step's own words where the
// case says more than its mark.
pub fn result_step(case: ResultCase, line: StepLine) -> &'static str {
    match (line.step, line.mark) {
        (Step::Write, Mark::Done) => "書き込み完了",
        (Step::Write, _) => match case {
            ResultCase::WriteFailed { .. } => "書き込みを完了できませんでした",
            ResultCase::WriteCancelled { .. } => "中止（書き込みは完了していません）",
            _ => mark_label(line.mark),
        },
        (Step::Verify, _) => match case {
            ResultCase::Verified(mode) => match mode {
                VerifyMode::Full => "完全検証完了",
                _ => "クイック検証完了",
            },
            ResultCase::WrittenWithoutVerify => "検証なし",
            ResultCase::VerifyCancelled => "検証は完了していません",
            ResultCase::VerifyMismatch => "読み戻したデータが一致しませんでした",
            ResultCase::VerifyNotCompleted(reason) => {
                if verify_ran(reason) {
                    "検証中に問題が発生しました"
                } else {
                    "検証を開始できませんでした"
                }
            }
            _ => mark_label(line.mark),
        },
        (Step::Prepare, _) => mark_label(line.mark),
    }
}

// Whether Verify had started reading when it failed (otherwise it could
// not start).
fn verify_ran(reason: Reason) -> bool {
    matches!(
        reason,
        Reason::VerifyReadError
            | Reason::VerifyLengthMismatch
            | Reason::QuickVerifyUnsupported
            | Reason::ImageChanged
    )
}

// What the result means for the USB drive now, and what to do. Built only
// from the case's typed reasons, never from an error's own text.
pub fn result_message(case: ResultCase) -> String {
    const INCOMPLETE: &str = "USB ドライブには不完全なイメージが残っている可能性があります。起動用の USB ドライブとして使用しないでください。";
    const NOTHING_WRITTEN: &str = "USB ドライブには何も書き込んでいません。";
    match case {
        ResultCase::Verified(_) => "USB ドライブを使用できます。".to_string(),
        ResultCase::WrittenWithoutVerify => "書き込み後の検証は行っていません。".to_string(),
        ResultCase::VerifyCancelled => "書き込み後の検証は完了していません。".to_string(),
        ResultCase::VerifyMismatch => "書き込みは完了しましたが、読み戻したデータがイメージと一致しませんでした。正しく書き込まれていない可能性があるため、この USB ドライブを起動用として使用することはおすすめしません。".to_string(),
        ResultCase::VerifyNotCompleted(reason) => format!(
            "書き込みは完了していますが、検証を完了できませんでした。\n{}",
            reason_text(reason)
        ),
        ResultCase::WriteFailed {
            reason,
            target_modified,
        } => format!(
            "{}\n{}",
            reason_text(reason),
            if target_modified {
                INCOMPLETE
            } else {
                NOTHING_WRITTEN
            }
        ),
        ResultCase::WriteCancelled { target_modified } => if target_modified {
            INCOMPLETE
        } else {
            NOTHING_WRITTEN
        }
        .to_string(),
        ResultCase::CancelledBeforeWrite => {
            "USB ドライブへの書き込みは開始されていません。".to_string()
        }
        ResultCase::NotStarted(reason) => format!("{}\n{NOTHING_WRITTEN}", reason_text(reason)),
        ResultCase::Lost => format!("内部エラーで処理が終了しました。\n{INCOMPLETE}"),
    }
}

pub fn result_action(action: ResultAction) -> &'static str {
    match action {
        ResultAction::SafeRemoval => "安全に取り外す",
        ResultAction::WriteAnother => "別の USB に書き込む",
        ResultAction::WriteAgain => "もう一度書き込む",
        ResultAction::Retry => "やり直す",
        ResultAction::BackToMain => "メイン画面に戻る",
        ResultAction::Done => "完了",
    }
}

// The target as the operation and result views name it: its name and
// device node (never its serial number or object path).
pub fn target_summary(target: &DeviceDisplay) -> String {
    format!(
        "{} · {}",
        device_name(&target.vendor, &target.model),
        target.device
    )
}

pub fn reason_text(reason: Reason) -> &'static str {
    match reason {
        Reason::TargetNotFound => "書き込み先の USB ドライブが見つかりません。",
        Reason::TargetUnreadable => "書き込み先の USB ドライブの情報を取得できませんでした。",
        Reason::TargetChanged => {
            "書き込み先の USB ドライブを、選択したものと同じだと確認できませんでした。"
        }
        Reason::TargetNotSelectable => {
            "書き込み先の USB ドライブは、現在は書き込み先に選べません。"
        }
        Reason::ImageUnreadable => "イメージファイルを読み取れませんでした。",
        Reason::ImageRefused => "このイメージは書き込めません。",
        Reason::QuickVerifyUnsupported => "このイメージではクイック検証を利用できません。",
        Reason::CompressedImageDamaged => "圧縮イメージが壊れているか、不完全です。",
        Reason::CompressedImageTooLarge => {
            "展開後のサイズが、書き込み先の USB ドライブの容量を超えています。"
        }
        Reason::CompressedImageTooDemanding => {
            "この圧縮イメージは、展開に必要な資源が多すぎるため書き込めません。"
        }
        Reason::ImageChanged => "確認中にイメージファイルが変更されました。",
        Reason::ConfirmationFailed => {
            "最終確認の内容と一致しなかったため、書き込みを開始しませんでした。"
        }
        Reason::TargetRecheckFailed => {
            "書き込み直前の再確認で、書き込み先の USB ドライブに問題が見つかりました。"
        }
        Reason::AccessDenied => "USB ドライブへのアクセスが許可されませんでした。",
        Reason::AuthenticationCancelled => {
            "認証がキャンセルされたため、USB ドライブを開けませんでした。"
        }
        Reason::DeviceBusyOrRefused => "USB ドライブを開けませんでした。使用中の可能性があります。",
        Reason::SystemServiceUnavailable => {
            "システムのディスク管理サービス（UDisks2）と通信できませんでした。"
        }
        Reason::OpenedDeviceMismatch => {
            "開いたデバイスを書き込み先と同じだと確認できなかったため、書き込みを開始しませんでした。"
        }
        Reason::WriteError => "USB ドライブへの書き込み中にエラーが発生しました。",
        Reason::ImageChangedDuringWrite => "書き込み中にイメージファイルが変更されました。",
        Reason::SyncError => "書き込みの仕上げ（データの確定）を完了できませんでした。",
        Reason::VerifyTargetChanged => {
            "検証の前の再確認で、USB ドライブを書き込み先と同じだと確認できませんでした。"
        }
        Reason::VerifyOpenFailed => "検証のために USB ドライブを開けませんでした。",
        Reason::VerifyDirectReadUnavailable => {
            "この環境では、検証に必要な直接読み取りを利用できませんでした。"
        }
        Reason::VerifyNotStarted => "検証が開始されませんでした。",
        Reason::VerifyMismatch => {
            "書き込んだデータがイメージと一致しませんでした。この USB ドライブは正しく書き込まれていない可能性があります。"
        }
        Reason::VerifyReadError => "読み戻し中にエラーが発生しました。",
        Reason::VerifyLengthMismatch => "読み戻したデータの長さが一致しませんでした。",
    }
}

// The final confirmation, built only from the worker's request (what the
// operation itself selected and opened) and the name of the file the
// request named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationText {
    pub image_name: String,
    // (label, value) lines under the image name.
    pub image_lines: Vec<(&'static str, String)>,
    pub target_name: String,
    pub target_line: String,
    pub verify: &'static str,
    pub warning: String,
}

pub fn confirmation(
    request: &WorkerConfirmationRequest,
    image_name: &str,
    compression: Option<CompressionFormat>,
    compressed_size: Option<u64>,
) -> ConfirmationText {
    let target = &request.target;
    let target_name = device_name(&target.vendor, &target.model);
    let image_lines = match compression {
        None => vec![("サイズ", size(request.image_size))],
        Some(format) => {
            let mut lines = vec![("形式", image_kind(Some(format)).to_string())];
            if let Some(compressed) = compressed_size {
                lines.push(("圧縮ファイル", size(compressed)));
            }
            lines.push(("書き込みサイズ", size(request.image_size)));
            lines
        }
    };
    ConfirmationText {
        image_name: image_name.to_string(),
        image_lines,
        target_line: format!(
            "{} · {} · {}",
            target.device,
            size(target.size),
            bus(&target.connection_bus)
        ),
        warning: format!(
            "{target_name}（{}）のデータはすべて消去されます。",
            target.device
        ),
        target_name,
        verify: verify_title(request.verify_mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_usb_writer::report::UnsupportedCompression;
    use linux_usb_writer::{DeviceDisplay, RiskLevel, SafetyAssessment};

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(512), "512 バイト");
        assert_eq!(size(8_054_112_256), "8.1 GB");
        assert_eq!(size(620_000_000), "620.0 MB");
        assert_eq!(exact_bytes(8_054_112_256), "8,054,112,256 バイト");
        assert_eq!(exact_bytes(999), "999 バイト");
    }

    #[test]
    fn image_refusals_are_explained_without_type_names() {
        let cases = [
            ImageSourceError::Io(io::Error::from(io::ErrorKind::NotFound)),
            ImageSourceError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            ImageSourceError::Io(io::Error::other("x")),
            ImageSourceError::NotRegularFile,
            ImageSourceError::UnsupportedFormat(UnsupportedCompression::Zstd),
            ImageSourceError::ExtensionMismatch {
                expected: CompressionFormat::Gzip,
            },
        ];
        for error in &cases {
            let text = image_error(error);
            assert!(!text.is_empty());
            for type_name in [
                "Io",
                "NotRegularFile",
                "UnsupportedFormat",
                "ExtensionMismatch",
            ] {
                assert!(!text.contains(type_name), "{text}");
            }
        }
        assert!(image_error(&cases[4]).contains("ZSTD"));
    }

    #[test]
    fn only_the_selected_verify_mode_is_explained() {
        assert_eq!(verify_help(false, VerifyMode::Quick), None);
        assert_eq!(
            verify_help(true, VerifyMode::Quick),
            Some("書き込み後、一部を読み戻して確認します。")
        );
        assert_eq!(
            verify_help(true, VerifyMode::Full),
            Some(
                "書き込んだデータをすべて読み戻して確認します。\nクイック検証より時間がかかります。"
            )
        );
        assert_eq!(
            verify_help(true, VerifyMode::None),
            Some("書き込み後の読み戻し確認は行われません。")
        );
        // Each mode has its own explanation.
        let modes = [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None];
        for a in modes {
            for b in modes {
                assert_eq!(a == b, verify_help(true, a) == verify_help(true, b));
            }
        }
    }

    #[test]
    fn a_protected_device_says_why() {
        assert_eq!(
            protection(&[RiskReason::CriticalMount, RiskReason::SystemDevice]),
            "システム領域（/ や /boot など）が使用しています。システムのディスクです。"
        );
        // The informational reason alone explains nothing.
        assert_eq!(
            protection(&[RiskReason::UsbRemovable]),
            "安全のため選択できません。"
        );
    }

    #[test]
    fn transfers_use_the_totals_unit() {
        assert_eq!(transfer(1_150_000_000, 1_800_000_000), "1.15 GB / 1.80 GB");
        assert_eq!(transfer(0, 620_000_000), "0.00 MB / 620.00 MB");
        assert_eq!(transfer(12, 512), "12 / 512 バイト");
    }

    // The request as the worker sends it: its values are the operation's
    // own, fresh ones.
    fn request(verify_mode: VerifyMode) -> WorkerConfirmationRequest {
        WorkerConfirmationRequest {
            target: DeviceDisplay {
                device: "/dev/sdq".to_string(),
                vendor: "Fresh".to_string(),
                model: "Drive".to_string(),
                serial: "S".to_string(),
                size: 16_000_000_000,
                connection_bus: "usb".to_string(),
                removable: true,
                read_only: false,
                media_available: true,
                mount_points: Vec::new(),
            },
            block_path: "/org/freedesktop/UDisks2/block_devices/sdq".to_string(),
            diskseq: Some(7),
            assessment: SafetyAssessment {
                risk_level: RiskLevel::Normal,
                writable: true,
                reasons: Vec::new(),
            },
            image_size: 3_800_000_000,
            verify_mode,
            expected_text: "/dev/sdq".to_string(),
        }
    }

    #[test]
    fn the_confirmation_shows_the_requests_values() {
        let text = confirmation(&request(VerifyMode::Quick), "os.img", None, None);
        assert_eq!(text.image_name, "os.img");
        assert_eq!(text.image_lines, vec![("サイズ", "3.8 GB".to_string())]);
        assert_eq!(text.target_name, "Fresh Drive");
        assert_eq!(text.target_line, "/dev/sdq · 16.0 GB · USB");
        assert_eq!(text.verify, "クイック検証");
        assert_eq!(
            text.warning,
            "Fresh Drive（/dev/sdq）のデータはすべて消去されます。"
        );
        // The serial number is not shown.
        assert!(!format!("{text:?}").contains("\"S\""));
    }

    #[test]
    fn a_compressed_confirmation_shows_both_sizes() {
        let text = confirmation(
            &request(VerifyMode::Full),
            "os.img.xz",
            Some(CompressionFormat::Xz),
            Some(620_000_000),
        );
        assert_eq!(
            text.image_lines,
            vec![
                ("形式", "XZ 圧縮イメージ".to_string()),
                ("圧縮ファイル", "620.0 MB".to_string()),
                ("書き込みサイズ", "3.8 GB".to_string()),
            ]
        );
        assert_eq!(text.verify, "完全検証");
    }

    #[test]
    fn compressed_writes_say_they_decompress() {
        let mut tracker = Tracker::new(VerifyMode::Full);
        tracker.activity = Activity::Writing;
        assert_eq!(activity(&tracker).0, "書き込み中");
        tracker.compression = Some(CompressionFormat::Gzip);
        assert_eq!(activity(&tracker).0, "展開しながら書き込み中");
        tracker.activity = Activity::Syncing;
        assert_eq!(activity(&tracker).0, "書き込みを仕上げています…");
        tracker.cancel_requested = true;
        assert_eq!(activity(&tracker).0, "中止しています…");
    }

    #[test]
    fn verify_is_worded_as_checking_the_written_data() {
        let mut tracker = Tracker::new(VerifyMode::Quick);
        tracker.activity = Activity::Verifying;
        assert_eq!(
            activity(&tracker),
            ("クイック検証中", "書き込んだデータを確認しています。")
        );
        tracker.verify_mode = VerifyMode::Full;
        assert_eq!(
            activity(&tracker),
            ("完全検証中", "書き込んだデータをすべて確認しています。")
        );
        for text in [
            activity(&tracker).0,
            activity(&tracker).1,
            headline(Step::Verify),
        ] {
            assert!(!text.contains("安全"), "{text}");
        }
    }

    #[test]
    fn preparing_says_nothing_was_written_yet() {
        let tracker = Tracker::new(VerifyMode::Quick);
        assert_eq!(
            activity(&tracker).1,
            "まだ USB ドライブには書き込んでいません。"
        );
    }

    fn every_case() -> Vec<ResultCase> {
        vec![
            ResultCase::Verified(VerifyMode::Quick),
            ResultCase::Verified(VerifyMode::Full),
            ResultCase::WrittenWithoutVerify,
            ResultCase::VerifyCancelled,
            ResultCase::VerifyMismatch,
            ResultCase::VerifyNotCompleted(Reason::VerifyReadError),
            ResultCase::VerifyNotCompleted(Reason::VerifyTargetChanged),
            ResultCase::WriteFailed {
                reason: Reason::WriteError,
                target_modified: true,
            },
            ResultCase::WriteFailed {
                reason: Reason::SyncError,
                target_modified: false,
            },
            ResultCase::WriteCancelled {
                target_modified: true,
            },
            ResultCase::WriteCancelled {
                target_modified: false,
            },
            ResultCase::CancelledBeforeWrite,
            ResultCase::NotStarted(Reason::AccessDenied),
            ResultCase::Lost,
        ]
    }

    // Everything the result view says for a case, in one string.
    fn everything_said(case: ResultCase) -> String {
        let mut said = format!("{}\n{}", result_title(case), result_message(case));
        for (step, mark) in [Step::Prepare, Step::Write, Step::Verify]
            .into_iter()
            .flat_map(|step| {
                [
                    Mark::Waiting,
                    Mark::Done,
                    Mark::Skipped,
                    Mark::Cancelled,
                    Mark::Failed,
                ]
                .map(|mark| (step, mark))
            })
        {
            said.push('\n');
            said.push_str(result_step(case, StepLine { step, mark }));
        }
        said
    }

    fn line(step: Step, mark: Mark) -> StepLine {
        StepLine { step, mark }
    }

    #[test]
    fn success_says_what_was_verified() {
        let quick = ResultCase::Verified(VerifyMode::Quick);
        assert_eq!(result_title(quick), "完了しました");
        assert_eq!(
            result_step(quick, line(Step::Write, Mark::Done)),
            "書き込み完了"
        );
        assert_eq!(
            result_step(quick, line(Step::Verify, Mark::Done)),
            "クイック検証完了"
        );
        assert_eq!(result_message(quick), "USB ドライブを使用できます。");
        let full = ResultCase::Verified(VerifyMode::Full);
        assert_eq!(
            result_step(full, line(Step::Verify, Mark::Done)),
            "完全検証完了"
        );
    }

    #[test]
    fn verify_none_never_claims_a_verified_or_usable_drive() {
        let case = ResultCase::WrittenWithoutVerify;
        assert_eq!(result_title(case), "書き込みが完了しました");
        assert_eq!(
            result_step(case, line(Step::Verify, Mark::Skipped)),
            "検証なし"
        );
        assert_eq!(result_message(case), "書き込み後の検証は行っていません。");
        let said = everything_said(case);
        for forbidden in ["検証済み", "使用できます", "正常", "安全"] {
            assert!(!said.contains(forbidden), "{forbidden}: {said}");
        }
    }

    #[test]
    fn a_cancelled_verify_says_the_write_is_complete() {
        let case = ResultCase::VerifyCancelled;
        assert_eq!(result_title(case), "書き込みは完了しています");
        assert_eq!(
            result_step(case, line(Step::Verify, Mark::Cancelled)),
            "検証は完了していません"
        );
        assert_eq!(result_message(case), "書き込み後の検証は完了していません。");
        // Not worded as an error: the lines it actually shows.
        let said = [
            result_title(case).to_string(),
            result_message(case),
            result_step(case, line(Step::Write, Mark::Done)).to_string(),
            result_step(case, line(Step::Verify, Mark::Cancelled)).to_string(),
        ]
        .join("\n");
        for error_like in ["失敗", "エラー", "問題"] {
            assert!(!said.contains(error_like), "{said}");
        }
    }

    #[test]
    fn a_mismatch_is_not_called_a_failed_write() {
        let case = ResultCase::VerifyMismatch;
        assert_eq!(result_title(case), "検証で問題が見つかりました");
        assert_eq!(
            result_step(case, line(Step::Verify, Mark::Failed)),
            "読み戻したデータが一致しませんでした"
        );
        assert_eq!(
            result_step(case, line(Step::Write, Mark::Done)),
            "書き込み完了"
        );
        let message = result_message(case);
        assert!(message.contains("おすすめしません"), "{message}");
        let said = everything_said(case);
        assert!(!said.contains("書き込み失敗"), "{said}");
        assert!(!said.contains("書き込みを完了できませんでした"), "{said}");
    }

    #[test]
    fn a_verify_error_is_not_a_mismatch_and_the_write_is_complete() {
        let during = ResultCase::VerifyNotCompleted(Reason::VerifyReadError);
        assert_eq!(result_title(during), "検証を完了できませんでした");
        assert_eq!(
            result_step(during, line(Step::Verify, Mark::Failed)),
            "検証中に問題が発生しました"
        );
        assert!(result_message(during).starts_with("書き込みは完了しています"));
        assert!(!everything_said(during).contains("一致しませんでした"));
        // One that never started reading is not said to have run.
        let before = ResultCase::VerifyNotCompleted(Reason::VerifyOpenFailed);
        assert_eq!(
            result_step(before, line(Step::Verify, Mark::Failed)),
            "検証を開始できませんでした"
        );
    }

    #[test]
    fn a_failed_or_cancelled_write_warns_about_an_incomplete_image() {
        let failed = ResultCase::WriteFailed {
            reason: Reason::WriteError,
            target_modified: true,
        };
        assert_eq!(result_title(failed), "書き込みを完了できませんでした");
        assert!(result_message(failed).contains("不完全なイメージ"));
        assert!(result_message(failed).contains("起動用の USB ドライブとして使用しないでください"));

        let cancelled = ResultCase::WriteCancelled {
            target_modified: true,
        };
        assert_eq!(result_title(cancelled), "書き込みを中止しました");
        assert!(result_message(cancelled).contains("不完全なイメージ"));
        assert!(!everything_said(cancelled).contains("エラー"));
    }

    #[test]
    fn a_cancellation_before_the_write_says_it_never_started() {
        let case = ResultCase::CancelledBeforeWrite;
        assert_eq!(result_title(case), "書き込みを中止しました");
        assert_eq!(
            result_message(case),
            "USB ドライブへの書き込みは開始されていません。"
        );
    }

    // No result says the drive can be removed (only a completed Safe
    // Removal may), nor that nothing was changed on it by removal.
    #[test]
    fn no_result_speaks_for_safe_removal() {
        for case in every_case() {
            let said = everything_said(case);
            for forbidden in ["安全", "取り外", "電源", "何も変更"] {
                assert!(!said.contains(forbidden), "{case:?}: {said}");
            }
        }
        assert_eq!(result_action(ResultAction::SafeRemoval), "安全に取り外す");
    }

    // Result texts are built from typed reasons only: no type names, no
    // object paths, no serial numbers.
    #[test]
    fn results_show_no_raw_errors_or_identifiers() {
        for case in every_case() {
            let said = everything_said(case);
            for raw in ["Error", "/org/freedesktop", "UDisks2.", "Serial", "{", "::"] {
                assert!(!said.contains(raw), "{case:?}: {said}");
            }
        }
        let target = DeviceDisplay {
            device: "/dev/sdq".to_string(),
            vendor: "General".to_string(),
            model: "UDisk".to_string(),
            serial: "SERIAL-123".to_string(),
            size: 8_000_000_000,
            connection_bus: "usb".to_string(),
            removable: true,
            read_only: false,
            media_available: true,
            mount_points: Vec::new(),
        };
        assert_eq!(target_summary(&target), "General UDisk · /dev/sdq");
    }

    #[test]
    fn every_result_kind_has_its_own_icon() {
        let kinds = [
            ResultKind::Success,
            ResultKind::WrittenNotVerified,
            ResultKind::Cancelled,
            ResultKind::Failed,
        ];
        for a in kinds {
            for b in kinds {
                assert_eq!(a == b, result_icon(a) == result_icon(b));
            }
        }
    }

    #[test]
    fn marks_are_words() {
        // Marks are words, never only an icon.
        for mark in [
            Mark::Waiting,
            Mark::Active,
            Mark::Done,
            Mark::Skipped,
            Mark::Cancelled,
            Mark::Failed,
        ] {
            assert!(!mark_label(mark).is_empty());
        }
        assert_eq!(mark_label(Mark::Skipped), "検証なし");
    }

    #[test]
    fn a_skipped_verify_looks_neither_failed_nor_cancelled() {
        let marks = [
            Mark::Waiting,
            Mark::Active,
            Mark::Done,
            Mark::Skipped,
            Mark::Cancelled,
            Mark::Failed,
        ];
        // Every mark has its own icon.
        for a in marks {
            for b in marks {
                assert_eq!(a == b, mark_icon(a) == mark_icon(b), "{a:?} {b:?}");
            }
        }
        // Not a cross, a stop or an error sign (Breeze draws
        // `list-remove-symbolic` as a red cross).
        for failure_like in [
            "list-remove-symbolic",
            "process-stop-symbolic",
            "dialog-error-symbolic",
            "window-close-symbolic",
        ] {
            assert_ne!(mark_icon(Mark::Skipped), failure_like);
        }
    }
}
