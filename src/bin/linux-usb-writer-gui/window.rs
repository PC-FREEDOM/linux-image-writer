// The main window: IMAGE, TARGET, WRITE OPTIONS, the warning and "Write",
// top to bottom. State lives in `State` (presentation only); every section
// is drawn from it. Work that can block -- image inspection and the device
// list -- runs on GIO's blocking pool; only its result comes back to the GTK
// thread.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use linux_usb_writer::report::ImageSourceError;
use linux_usb_writer::{
    CandidateListError, DeviceCandidate, ImageInfo, TargetRef, VerifyAvailability, VerifyMode,
    inspect_image, list_candidates,
};

use crate::model::{
    self, CandidateLike, Eligibility, ImagePhase, SelectionOrigin, TargetChoice, VerifyState,
};
use crate::text;

// How often the device list is read again (and whenever the window becomes
// active).
const POLL_INTERVAL: Duration = Duration::from_secs(2);

const VERIFY_MODES: [VerifyMode; 3] = [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None];

enum ImageState {
    Missing,
    Inspecting {
        name: String,
    },
    Ready {
        name: String,
        // What the write request will name (Phase 3B-1b-2); never shown.
        #[allow(dead_code)]
        path: PathBuf,
        info: ImageInfo,
    },
    Failed {
        name: String,
        message: String,
        detail: String,
    },
}

impl ImageState {
    fn phase(&self) -> ImagePhase {
        match self {
            ImageState::Missing => ImagePhase::Missing,
            ImageState::Inspecting { .. } => ImagePhase::Inspecting,
            ImageState::Failed { .. } => ImagePhase::Invalid,
            ImageState::Ready { info, .. } => ImagePhase::Ready {
                logical_size: info.logical_size(),
            },
        }
    }

    fn info(&self) -> Option<&ImageInfo> {
        match self {
            ImageState::Ready { info, .. } => Some(info),
            _ => None,
        }
    }
}

enum Discovery {
    // Before the first answer.
    Loading,
    Listed,
    // The latest attempt failed; the previous list is still shown.
    Failed { detail: String },
}

struct State {
    image: ImageState,
    // Increases with every image choice; an inspection result for an older
    // choice is dropped.
    image_generation: u64,
    discovery: Discovery,
    candidates: Vec<DeviceCandidate>,
    choice: TargetChoice<TargetRef>,
    selected: Option<usize>,
    refreshing: bool,
    verify: VerifyState,
    // What the target section shows now, to redraw only on a change.
    target_view: Option<TargetView>,
    protected_expanded: bool,
    details_expanded: bool,
}

struct Ui {
    window: adw::ApplicationWindow,
    image_box: gtk::Box,
    target_box: gtk::Box,
    verify_box: gtk::Box,
    write_button: gtk::Button,
    write_status: gtk::Label,
    // The groups the target and Verify choices join. Never shown and kept
    // for the window's lifetime, so each choice is a radio button (one of
    // many, never unchecked by clicking it) even when a list has only one.
    target_group: gtk::CheckButton,
    verify_group: gtk::CheckButton,
    state: RefCell<State>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

// Shows the window (creating it once) and inspects `image` if given.
pub fn present(app: &adw::Application, image: Option<PathBuf>) {
    let ui = UI.with(|slot| slot.borrow().clone()).unwrap_or_else(|| {
        let ui = build(app);
        UI.with(|slot| *slot.borrow_mut() = Some(ui.clone()));
        ui
    });
    ui.window.present();
    if let Some(path) = image {
        inspect(&ui, path);
    }
}

fn build(app: &adw::Application) -> Rc<Ui> {
    let image_box = section_box();
    let target_box = section_box();
    let verify_box = section_box();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&group("イメージ", &image_box));
    content.append(&group("書き込み先", &target_box));
    content.append(&group("書き込みオプション", &verify_box));

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(
            &adw::Clamp::builder()
                .maximum_size(720)
                .child(&content)
                .build(),
        )
        .build();

    // The warning and "Write" stay visible below the scrolling sections.
    let warning = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    warning.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    warning.append(
        &gtk::Label::builder()
            .label("書き込み先のデータはすべて消去されます。")
            .wrap(true)
            .build(),
    );
    let write_status = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();
    let write_button = gtk::Button::builder()
        .label("USB に書き込む")
        .halign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["pill", "destructive-action"])
        .build();
    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    bottom.append(&warning);
    bottom.append(&write_status);
    bottom.append(&write_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    toolbar.add_bottom_bar(&bottom);
    toolbar.set_bottom_bar_style(adw::ToolbarStyle::RaisedBorder);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Linux USB Writer")
        .default_width(640)
        .default_height(760)
        .content(&toolbar)
        .build();

    let ui = Rc::new(Ui {
        window,
        image_box,
        target_box,
        verify_box,
        write_button,
        write_status,
        target_group: gtk::CheckButton::new(),
        verify_group: gtk::CheckButton::new(),
        state: RefCell::new(State {
            image: ImageState::Missing,
            image_generation: 0,
            discovery: Discovery::Loading,
            candidates: Vec::new(),
            choice: TargetChoice::default(),
            selected: None,
            refreshing: false,
            verify: VerifyState::initial(),
            target_view: None,
            protected_expanded: false,
            details_expanded: false,
        }),
    });

    {
        let ui_ref = ui.clone();
        ui.write_button
            .connect_clicked(move |_| show_not_connected_yet(&ui_ref));
    }
    {
        let ui_ref = ui.clone();
        glib::timeout_add_local(POLL_INTERVAL, move || {
            refresh_targets(&ui_ref);
            glib::ControlFlow::Continue
        });
    }
    {
        let ui_ref = ui.clone();
        ui.window.connect_is_active_notify(move |window| {
            if window.is_active() {
                refresh_targets(&ui_ref);
            }
        });
    }

    render_all(&ui);
    refresh_targets(&ui);
    ui
}

fn section_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build()
}

fn group(title: &str, child: &gtk::Box) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    group.add(child);
    group
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().build();
    row.set_use_markup(false);
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

// A row with a status icon: the icon is decoration, the text says it all.
fn status_row(icon: &str, title: &str, subtitle: &str) -> adw::ActionRow {
    let row = row(title, subtitle);
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

fn detail_row(title: &str, value: &str) -> adw::ActionRow {
    let row = row(title, value);
    row.add_css_class("property");
    row.set_subtitle_selectable(true);
    row
}

fn details(title: &str, rows: &[(&str, String)]) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().build();
    expander.set_use_markup(false);
    expander.set_title(title);
    for (name, value) in rows {
        expander.add_row(&detail_row(name, value));
    }
    expander
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build()
}

fn render_all(ui: &Rc<Ui>) {
    render_image(ui);
    render_targets(ui, true);
    render_verify(ui);
    render_write(ui);
}

// ---- IMAGE ----

fn choose_image(ui: &Rc<Ui>) {
    let images = gtk::FileFilter::new();
    images.set_name(Some("ディスクイメージ（ISO / IMG / GZIP / XZ）"));
    for suffix in ["iso", "img", "gz", "xz"] {
        images.add_suffix(suffix);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some("すべてのファイル"));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&images);
    filters.append(&all);

    // The file chooser portal inside Flatpak; the format is decided by
    // inspection, never by these filters.
    let dialog = gtk::FileDialog::builder()
        .title("書き込むイメージを選択")
        .modal(true)
        .filters(&filters)
        .default_filter(&images)
        .build();
    let ui_ref = ui.clone();
    dialog.open(Some(&ui.window), gio::Cancellable::NONE, move |result| {
        if let Ok(path) = result.map(|file| file.path())
            && let Some(path) = path
        {
            inspect(&ui_ref, path);
        }
    });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn inspect(ui: &Rc<Ui>, path: PathBuf) {
    let generation = {
        let mut state = ui.state.borrow_mut();
        state.image_generation += 1;
        state.image = ImageState::Inspecting {
            name: file_name(&path),
        };
        state.image_generation
    };
    render_all(ui);

    let ui_ref = ui.clone();
    glib::spawn_future_local(async move {
        let inspected_path = path.clone();
        let result = gio::spawn_blocking(move || inspect_image(&inspected_path)).await;
        {
            let mut state = ui_ref.state.borrow_mut();
            if state.image_generation != generation {
                return;
            }
            let name = file_name(&path);
            state.image = match result {
                Ok(Ok(info)) => {
                    state.verify = state
                        .verify
                        .for_image(|mode| info.verify_availability(mode));
                    ImageState::Ready { name, path, info }
                }
                Ok(Err(error)) => failed(name, &error),
                Err(_) => ImageState::Failed {
                    name,
                    message: "イメージを確認できませんでした".to_string(),
                    detail: "inspection panicked".to_string(),
                },
            };
            reconcile_targets(&mut state);
        }
        render_all(&ui_ref);
    });
}

fn failed(name: String, error: &ImageSourceError) -> ImageState {
    ImageState::Failed {
        name,
        message: text::image_error(error),
        detail: format!("{error:?}"),
    }
}

fn render_image(ui: &Rc<Ui>) {
    let state = ui.state.borrow();
    clear(&ui.image_box);
    let rows = list();
    let choose = |label: &str| {
        let button = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build();
        let ui_ref = ui.clone();
        button.connect_clicked(move |_| choose_image(&ui_ref));
        button
    };

    match &state.image {
        ImageState::Missing => {
            let row = status_row(
                "document-open-symbolic",
                "書き込むイメージを選択",
                "ISO / IMG / GZIP / XZ に対応しています",
            );
            let button = choose("イメージを選択");
            button.add_css_class("suggested-action");
            row.add_suffix(&button);
            rows.append(&row);
            ui.image_box.append(&rows);
        }
        ImageState::Inspecting { name } => {
            let row = row("イメージを確認しています…", name);
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.update_property(&[gtk::accessible::Property::Label("確認中")]);
            row.add_prefix(&spinner);
            row.add_suffix(&choose("別のイメージを選択"));
            rows.append(&row);
            ui.image_box.append(&rows);
        }
        ImageState::Ready { name, info, .. } => {
            let summary = format!(
                "書き込みできます · {} · {}",
                text::size(info.file_size()),
                text::image_kind(info.compression())
            );
            let row = status_row("emblem-ok-symbolic", name, &summary);
            row.add_suffix(&choose("変更"));
            rows.append(&row);
            if info.compression().is_some() {
                rows.append(&status_row(
                    "dialog-information-symbolic",
                    "展開しながら USB に書き込みます",
                    "書き込みサイズは準備時に確認します",
                ));
            }
            let verify = |mode| match info.verify_availability(mode) {
                VerifyAvailability::Available => "Available".to_string(),
                VerifyAvailability::Unavailable(reason) => {
                    format!("Unavailable ({})", text::verify_unavailable_name(reason))
                }
            };
            rows.append(&details(
                "技術情報",
                &[
                    (
                        "圧縮 (Compression)",
                        text::compression_name(info.compression()).to_string(),
                    ),
                    ("ファイルサイズ", text::exact_bytes(info.file_size())),
                    (
                        "書き込みサイズ",
                        info.logical_size()
                            .map(text::exact_bytes)
                            .unwrap_or_else(|| "準備時に確認（Preflight）".to_string()),
                    ),
                    ("Access", text::access_name(info.access()).to_string()),
                    ("Quick Verify", verify(VerifyMode::Quick)),
                    ("Full Verify", verify(VerifyMode::Full)),
                    ("Verify なし", verify(VerifyMode::None)),
                    ("アーキテクチャ / ブート方式", "Not detected".to_string()),
                ],
            ));
            ui.image_box.append(&rows);
        }
        ImageState::Failed {
            name,
            message,
            detail,
        } => {
            let row = status_row(
                "dialog-error-symbolic",
                "このイメージは書き込めません",
                &format!("{name} — {message}"),
            );
            row.add_suffix(&choose("別のイメージを選択"));
            rows.append(&row);
            rows.append(&details("技術情報", &[("エラー", detail.clone())]));
            ui.image_box.append(&rows);
        }
    }
}

// ---- TARGET ----

fn refresh_targets(ui: &Rc<Ui>) {
    {
        let mut state = ui.state.borrow_mut();
        if state.refreshing {
            return;
        }
        state.refreshing = true;
    }
    let ui_ref = ui.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(list_candidates).await;
        {
            let mut state = ui_ref.state.borrow_mut();
            state.refreshing = false;
            match result {
                Ok(Ok(candidates)) => {
                    state.candidates = candidates;
                    state.discovery = Discovery::Listed;
                    reconcile_targets(&mut state);
                }
                Ok(Err(error)) => state.discovery = listing_failed(&error),
                Err(_) => {
                    state.discovery = Discovery::Failed {
                        detail: "device listing panicked".to_string(),
                    }
                }
            }
        }
        render_targets(&ui_ref, false);
        render_write(&ui_ref);
    });
}

fn listing_failed(error: &CandidateListError) -> Discovery {
    Discovery::Failed {
        detail: format!("{error:?}"),
    }
}

fn image_context(state: &State) -> model::ImageContext {
    state.image.phase().context()
}

fn reconcile_targets(state: &mut State) {
    let context = image_context(state);
    let choice = std::mem::take(&mut state.choice);
    let reconciled = model::reconcile(choice, &state.candidates, context);
    state.choice = reconciled.choice;
    state.selected = reconciled.index;
}

fn select_target(ui: &Rc<Ui>, index: usize) {
    {
        let mut state = ui.state.borrow_mut();
        let context = image_context(&state);
        let Some(candidate) = state.candidates.get(index) else {
            return;
        };
        let Some(choice) = model::select_manually(candidate, context) else {
            return;
        };
        state.choice = choice;
        state.selected = Some(index);
    }
    // Redraw after this signal handler has returned.
    let ui_ref = ui.clone();
    glib::idle_add_local_once(move || {
        render_targets(&ui_ref, false);
        render_write(&ui_ref);
    });
}

// Everything the target section shows, compared to skip identical redraws
// (the list is read every few seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetView {
    image_ready: bool,
    loading: bool,
    failure: Option<String>,
    cleared: Option<&'static str>,
    available: Vec<EntryView>,
    too_small: Vec<EntryView>,
    protected: Vec<EntryView>,
    selected: Option<usize>,
    auto: bool,
    details: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryView {
    index: usize,
    title: String,
    subtitle: String,
}

fn target_view(state: &State) -> TargetView {
    let context = image_context(state);
    let mut view = TargetView {
        image_ready: context != model::ImageContext::NotReady,
        loading: matches!(state.discovery, Discovery::Loading),
        failure: match &state.discovery {
            Discovery::Failed { detail } => Some(detail.clone()),
            _ => None,
        },
        cleared: state.choice.cleared.map(text::clear_reason),
        available: Vec::new(),
        too_small: Vec::new(),
        protected: Vec::new(),
        selected: state.selected,
        auto: state
            .choice
            .selection
            .as_ref()
            .is_some_and(|selection| selection.origin == SelectionOrigin::Auto),
        details: Vec::new(),
    };
    for (index, candidate) in state.candidates.iter().enumerate() {
        let display = candidate.display();
        let place = format!(
            "{} · {} · {}",
            display.device,
            text::size(display.size),
            text::bus(&display.connection_bus)
        );
        let title = text::device_name(&display.vendor, &display.model);
        match model::eligibility(candidate, context) {
            Eligibility::Available => view.available.push(EntryView {
                index,
                title,
                subtitle: place,
            }),
            Eligibility::TooSmall { shortfall } => view.too_small.push(EntryView {
                index,
                title,
                subtitle: format!("{place} · 容量が {} 不足しています", text::size(shortfall)),
            }),
            Eligibility::Protected => view.protected.push(EntryView {
                index,
                title,
                subtitle: format!(
                    "{place}\n{}",
                    text::protection(&candidate.assessment().reasons)
                ),
            }),
        }
    }
    if let Some(candidate) = state.selected.and_then(|index| state.candidates.get(index)) {
        view.details = target_details(candidate);
    }
    view
}

// The selected device's technical details, as the library reports them
// (the serial number is left out).
fn target_details(candidate: &DeviceCandidate) -> Vec<(&'static str, String)> {
    let display = candidate.display();
    let assessment = candidate.assessment();
    let yes_no = |value: bool| if value { "はい" } else { "いいえ" }.to_string();
    let reasons = assessment
        .reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (major, minor) = candidate.major_minor();
    vec![
        ("デバイス", display.device.clone()),
        ("ベンダー", display.vendor.clone()),
        ("モデル", display.model.clone()),
        ("容量", text::exact_bytes(display.size)),
        ("接続", text::bus(&display.connection_bus)),
        ("リムーバブル", yes_no(display.removable)),
        ("書き込み可能", yes_no(assessment.writable)),
        (
            "Safety",
            format!(
                "{} ({reasons})",
                text::risk_level_name(&assessment.risk_level)
            ),
        ),
        (
            "マウント",
            if display.mount_points.is_empty() {
                "なし".to_string()
            } else {
                display.mount_points.join(", ")
            },
        ),
        (
            "diskseq",
            candidate
                .diskseq()
                .map(|seq| seq.to_string())
                .unwrap_or_else(|| "不明".to_string()),
        ),
        ("major:minor", format!("{major}:{minor}")),
    ]
}

fn render_targets(ui: &Rc<Ui>, force: bool) {
    let view = target_view(&ui.state.borrow());
    if !force && ui.state.borrow().target_view.as_ref() == Some(&view) {
        return;
    }
    let (protected_expanded, details_expanded) = {
        let state = ui.state.borrow();
        (state.protected_expanded, state.details_expanded)
    };
    clear(&ui.target_box);

    if let Some(detail) = &view.failure {
        let rows = list();
        rows.append(&status_row(
            "dialog-error-symbolic",
            text::candidate_list_error_message(),
            "しばらくすると自動的に再試行します",
        ));
        rows.append(&details("技術情報", &[("エラー", detail.clone())]));
        ui.target_box.append(&rows);
    }
    if let Some(cleared) = view.cleared {
        let rows = list();
        rows.append(&status_row("dialog-warning-symbolic", cleared, ""));
        ui.target_box.append(&rows);
    }

    if view.available.is_empty() {
        let rows = list();
        if view.loading {
            let row = row("USB ドライブを探しています…", "");
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.update_property(&[gtk::accessible::Property::Label("検索中")]);
            row.add_prefix(&spinner);
            rows.append(&row);
        } else if view.too_small.is_empty() {
            rows.append(&status_row(
                "drive-removable-media-symbolic",
                "USB ドライブが見つかりません",
                "書き込み先の USB ドライブを接続してください",
            ));
        } else {
            rows.append(&status_row(
                "drive-removable-media-symbolic",
                "このイメージを書き込める USB ドライブがありません",
                "容量の大きい USB ドライブを接続してください",
            ));
        }
        ui.target_box.append(&rows);
    } else {
        if !view.image_ready {
            ui.target_box
                .append(&caption("イメージを選択した後に書き込み先を確認します"));
        } else if view.selected.is_none() && view.available.len() > 1 {
            ui.target_box
                .append(&caption("書き込み先の USB ドライブを選択してください"));
        }
        let rows = list();
        for entry in &view.available {
            let row = row(&entry.title, &entry.subtitle);
            if view.image_ready {
                let check = gtk::CheckButton::new();
                check.set_group(Some(&ui.target_group));
                check.set_active(view.selected == Some(entry.index));
                check.update_property(&[gtk::accessible::Property::Label(&format!(
                    "{} {}",
                    entry.title, entry.subtitle
                ))]);
                let ui_ref = ui.clone();
                let index = entry.index;
                check.connect_toggled(move |check| {
                    if check.is_active() {
                        select_target(&ui_ref, index);
                    }
                });
                row.add_prefix(&check);
                row.set_activatable_widget(Some(&check));
                if view.selected == Some(entry.index) && view.auto {
                    row.add_suffix(&tag("自動的に選択しました"));
                }
            } else {
                // Before an image is ready nothing can be chosen; a choice kept
                // from an earlier image (while another one is inspected or was
                // refused) is still shown as chosen.
                row.add_prefix(&gtk::Image::from_icon_name(
                    "drive-removable-media-symbolic",
                ));
                row.add_suffix(&tag(if view.selected == Some(entry.index) {
                    "選択中"
                } else {
                    "検出済み"
                }));
            }
            rows.append(&row);
        }
        ui.target_box.append(&rows);
    }

    if !view.details.is_empty() {
        let rows = list();
        let expander = details(
            "選択中のデバイスの詳細",
            &view
                .details
                .iter()
                .map(|(name, value)| (*name, value.clone()))
                .collect::<Vec<_>>(),
        );
        expander.set_expanded(details_expanded);
        let ui_ref = ui.clone();
        expander.connect_expanded_notify(move |expander| {
            ui_ref.state.borrow_mut().details_expanded = expander.is_expanded();
        });
        rows.append(&expander);
        ui.target_box.append(&rows);
    }

    if !view.too_small.is_empty() {
        ui.target_box.append(&caption("容量不足"));
        let rows = list();
        for entry in &view.too_small {
            let row = status_row(
                "drive-removable-media-symbolic",
                &entry.title,
                &entry.subtitle,
            );
            rows.append(&row);
        }
        ui.target_box.append(&rows);
    }

    if !view.protected.is_empty() {
        let rows = list();
        let expander = adw::ExpanderRow::builder().build();
        expander.set_use_markup(false);
        expander.set_title(&format!(
            "保護されているデバイス（{}）",
            view.protected.len()
        ));
        expander.set_subtitle("安全のため、書き込み先には選べません");
        expander.add_prefix(&gtk::Image::from_icon_name("changes-prevent-symbolic"));
        for entry in &view.protected {
            expander.add_row(&row(&entry.title, &entry.subtitle));
        }
        expander.set_expanded(protected_expanded);
        let ui_ref = ui.clone();
        expander.connect_expanded_notify(move |expander| {
            ui_ref.state.borrow_mut().protected_expanded = expander.is_expanded();
        });
        rows.append(&expander);
        ui.target_box.append(&rows);
    }

    ui.state.borrow_mut().target_view = Some(view);
}

fn tag(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label", "caption"])
        .build()
}

// ---- WRITE OPTIONS ----

fn render_verify(ui: &Rc<Ui>) {
    let state = ui.state.borrow();
    clear(&ui.verify_box);
    let info = state.image.info();
    if info.is_none() {
        ui.verify_box
            .append(&caption("イメージを選択すると検証方法を設定できます"));
    }

    let rows = list();
    for mode in VERIFY_MODES {
        let availability = info.map(|info| info.verify_availability(mode));
        let mut subtitle = text::verify_description(mode).to_string();
        if let Some(VerifyAvailability::Unavailable(reason)) = availability {
            subtitle = format!("{subtitle}\n{}", text::verify_unavailable(reason));
        }
        let row = row(text::verify_title(mode), &subtitle);
        let check = gtk::CheckButton::new();
        check.set_group(Some(&ui.verify_group));
        check.set_active(info.is_some() && state.verify.mode == mode);
        check.update_property(&[gtk::accessible::Property::Label(text::verify_title(mode))]);
        let usable = availability == Some(VerifyAvailability::Available);
        row.set_sensitive(usable);
        let ui_ref = ui.clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                choose_verify(&ui_ref, mode);
            }
        });
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        if mode == VerifyMode::Quick {
            row.add_suffix(&tag("おすすめ"));
        }
        rows.append(&row);
    }
    ui.verify_box.append(&rows);

    if let Some(notice) = state.verify.notice {
        let rows = list();
        rows.append(&status_row(
            "dialog-information-symbolic",
            &text::verify_notice(notice),
            "",
        ));
        ui.verify_box.append(&rows);
    }
    if info.is_some() {
        ui.verify_box.append(&caption(
            "利用できるかどうかはイメージの形式による判定です。検証そのものは、実行時にデバイスの状態によって失敗することがあります。",
        ));
    }
}

fn choose_verify(ui: &Rc<Ui>, mode: VerifyMode) {
    {
        let mut state = ui.state.borrow_mut();
        let available = state
            .image
            .info()
            .is_some_and(|info| info.verify_availability(mode) == VerifyAvailability::Available);
        if !available || (state.verify.mode == mode && state.verify.notice.is_none()) {
            return;
        }
        state.verify = VerifyState::chosen(mode);
    }
    let ui_ref = ui.clone();
    glib::idle_add_local_once(move || {
        render_verify(&ui_ref);
        render_write(&ui_ref);
    });
}

// ---- WRITE ----

fn readiness(state: &State) -> Result<(), model::WriteBlocker> {
    let target_size = state
        .selected
        .and_then(|index| state.candidates.get(index))
        .map(|candidate| candidate.size());
    let verify_available = state.image.info().is_some_and(|info| {
        info.verify_availability(state.verify.mode) == VerifyAvailability::Available
    });
    model::write_readiness(
        state.image.phase(),
        !matches!(state.discovery, Discovery::Failed { .. }),
        target_size,
        verify_available,
    )
}

fn render_write(ui: &Rc<Ui>) {
    let state = ui.state.borrow();
    match readiness(&state) {
        Ok(()) => {
            ui.write_button.set_sensitive(true);
            ui.write_status.set_text("準備ができました");
        }
        Err(blocker) => {
            ui.write_button.set_sensitive(false);
            ui.write_status.set_text(&text::write_blocker(blocker));
        }
    }
}

// TEMPORARY (Phase 3B-1b-1): the write operation is not connected yet.
// Pressing "Write" only says so; it opens no device and writes nothing.
// Phase 3B-1b-2 replaces this with the worker, the final confirmation and
// the progress view.
fn show_not_connected_yet(ui: &Rc<Ui>) {
    let body = {
        let state = ui.state.borrow();
        if readiness(&state).is_err() {
            return;
        }
        let image = match &state.image {
            ImageState::Ready { name, .. } => name.clone(),
            _ => return,
        };
        let target = state
            .selected
            .and_then(|index| state.candidates.get(index))
            .map(|candidate| {
                let display = candidate.display();
                format!(
                    "{}（{} · {}）",
                    text::device_name(&display.vendor, &display.model),
                    display.device,
                    text::size(display.size)
                )
            })
            .unwrap_or_default();
        format!(
            "イメージ: {image}\n書き込み先: {target}\n検証: {}\n\n\
             この開発段階では、書き込みの処理はまだ接続されていません。USB には何も書き込んでいません。",
            text::verify_title(state.verify.mode)
        )
    };
    let dialog = adw::AlertDialog::new(Some("書き込みは次の段階で接続します"), Some(&body));
    dialog.add_response("close", "閉じる");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(Some(&ui.window));
}
