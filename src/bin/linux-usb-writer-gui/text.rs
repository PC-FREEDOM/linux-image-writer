// What the GUI says, in words: the library's typed answers turned into
// short Japanese text for the normal view, and their exact names for the
// technical details. Presentation only -- every decision is the library's.

use std::io;

use linux_usb_writer::report::{CompressionFormat, ImageSourceError};
use linux_usb_writer::{ImageAccess, RiskLevel, RiskReason, VerifyMode, VerifyUnavailableReason};

use crate::model::{ClearReason, VerifyNotice, WriteBlocker};

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

pub fn clear_reason(reason: ClearReason) -> &'static str {
    match reason {
        ClearReason::Disappeared => {
            "選択していた USB ドライブが見つからなくなりました。書き込み先を選び直してください。"
        }
        ClearReason::NoLongerSelectable => {
            "選択していた USB ドライブは現在選択できません。書き込み先を選び直してください。"
        }
        ClearReason::TooSmall => {
            "選択していた USB ドライブはこのイメージには容量が足りません。書き込み先を選び直してください。"
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use linux_usb_writer::report::UnsupportedCompression;

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
}
