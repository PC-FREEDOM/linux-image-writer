// The main window. Its main view is IMAGE, TARGET, WRITE OPTIONS, the
// warning and "Write", top to bottom; "Write" switches to the operation
// view, which follows the library's write worker until it ends. State lives
// in `State` and `Operation` (presentation only); every section is drawn
// from it. Work that can block -- image inspection and the device list --
// runs on GIO's blocking pool, and the write runs on the worker's own
// thread; only their results and messages come back to the GTK thread.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use linux_image_writer::report::ImageSourceError;
use linux_image_writer::{
    CandidateListError, ConfirmationDecision, DeviceCandidate, ImageInfo, OperationOutcome,
    RemovalTarget, TargetRef, VerifyAvailability, VerifyMode, WorkerConfirmationRequest,
    WorkerMessage, WriteOperationRequest, WriteWorker, inspect_image, list_candidates,
    request_safe_removal, spawn_write_worker,
};

use crate::expansion::{self, Panel};
use crate::i18n::{fill, n_, tr};
use crate::model::{
    self, CandidateLike, Eligibility, ImagePhase, SelectionOrigin, TargetChoice, TargetReturn,
    VerifyState,
};
use crate::operation::{self, CancelAction, Ending, Mark, STEPS, Tracker};
use crate::progress::{self, OverallProgress};
use crate::result::{
    self, Leaving, RemovalNotice, RemovalPresentation, RemovalStatus, ResultAction, ResultKind,
};
use crate::text;

// How often the device list is read again (and whenever the window becomes
// active).
const POLL_INTERVAL: Duration = Duration::from_secs(2);

// How often the worker's messages are read while an operation runs.
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);

const MAIN_VIEW: &str = "main";
const OPERATION_VIEW: &str = "operation";

const VERIFY_MODES: [VerifyMode; 3] = [VerifyMode::Quick, VerifyMode::Full, VerifyMode::None];

enum ImageState {
    Missing,
    Inspecting {
        name: String,
    },
    Ready {
        name: String,
        // What the write request names; never shown.
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
    // The one details panel shown expanded, if any.
    open_panel: Option<Panel>,
    // A preview's example target list, shown instead of the device list
    // (`None` outside a preview).
    preview_targets: Option<PreviewTargets>,
}

// A preview's target section: what it shows, and the size of the example
// target it selects (for the write button's readiness only).
struct PreviewTargets {
    view: TargetView,
    target_size: Option<u64>,
}

// One write operation, from "Write" until the user returns to the main view.
struct Operation {
    // The library's worker; `None` once its outcome arrived (it is then
    // joined) or it was lost.
    worker: Option<WriteWorker>,
    tracker: Tracker,
    // The name of the file the request named.
    image_name: String,
    // Dialogs waiting for an answer; closed if the operation ends first.
    dialogs: Vec<adw::AlertDialog>,
    // How it ended, with the outcome for the technical details.
    ended: Option<(Ending, String)>,
    // Safe Removal for the finished operation: the worker's own answer
    // (`WriteWorker::removal_target`, read once `Finished` has arrived),
    // `Unavailable` until then.
    removal: RemovalPresentation<RemovalTarget>,
    // A preview's Safe Removal, shown in place of `removal`: it holds no
    // target, so it can only be drawn (`None` outside a preview).
    preview_removal: Option<RemovalPresentation<()>>,
}

// The operation view's widgets, built once and updated from `Operation`.
struct OperationUi {
    // The result's icon (shown once the operation has ended).
    icon: gtk::Image,
    title: gtk::Label,
    // The progress area: the bar and its percentage, then the activity
    // spinner and the phase detail under them.
    progress_card: gtk::Box,
    percent: gtk::Label,
    spinner: gtk::Spinner,
    status: gtk::Label,
    note: gtk::Label,
    image: gtk::Label,
    target: gtk::Label,
    // The three steps, side by side (準備 → 書き込み → 検証), in the same
    // place while the operation runs and on its result.
    phase_items: Vec<PhaseItem>,
    progress: gtk::ProgressBar,
    amount: gtk::Label,
    message: gtk::Label,
    // Safe Removal, apart from the operation's own result.
    removal: RemovalUi,
    details: Vec<adw::ActionRow>,
    cancel: gtk::Button,
    // The result's actions, one button each, shown as the result offers
    // them.
    actions: Vec<(ResultAction, gtk::Button)>,
    // The one row every result action sits in, side by side; hidden while
    // the operation runs.
    action_row: gtk::Box,
}

// One step in the row of steps: an icon, the step's name and its state in
// words, named "<step>: <state>" for assistive technologies.
struct PhaseItem {
    item: gtk::Box,
    name: String,
    icon: gtk::Image,
    state: gtk::Label,
}

// The result view's Safe Removal section: one status row (a spinner while
// it runs, else an icon) and, when the outcome says so, one more line.
struct RemovalUi {
    list: gtk::ListBox,
    status: adw::ActionRow,
    spinner: gtk::Spinner,
    icon: gtk::Image,
    extra: adw::ActionRow,
}

struct Ui {
    window: adw::ApplicationWindow,
    views: gtk::Stack,
    toasts: adw::ToastOverlay,
    op: OperationUi,
    operation: RefCell<Option<Operation>>,
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
    // The expander rows currently shown for each details panel.
    panels: RefCell<Vec<(Panel, adw::ExpanderRow)>>,
    state: RefCell<State>,
    // A development preview (debug builds only, see preview.rs): the window
    // shows a fixed state and refuses everything that would start real
    // work -- writing, Safe Removal, the device list, the file chooser.
    // Always `false` otherwise; only `present_preview` sets it.
    preview: bool,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

// Shows the window (creating it once) and inspects `image` if given.
pub fn present(app: &adw::Application, image: Option<PathBuf>) {
    let ui = UI.with(|slot| slot.borrow().clone()).unwrap_or_else(|| {
        let ui = build(app, false);
        UI.with(|slot| *slot.borrow_mut() = Some(ui.clone()));
        ui
    });
    ui.window.present();
    let Some(path) = image else {
        return;
    };
    // While an operation runs, or its result is shown, the main view is not
    // shown or changed: an image opened then is not taken (never queued),
    // and a toast says why.
    if ui.operation.borrow().is_none() {
        inspect(&ui, path);
    } else if let Some(message) = open_refused(&ui) {
        ui.toasts.add_toast(adw::Toast::new(&message));
    }
}

// Why an image opened from outside is not taken now, from the operation's
// own state: still processing (the operation, or Safe Removal), or showing
// its result. `None` when neither (no operation, or one going straight back
// to the main view).
fn open_refused(ui: &Ui) -> Option<String> {
    if operation_running(ui) || removal_running(ui) {
        return Some(text::open_refused(true));
    }
    let result_shown = ui.operation.borrow().as_ref().is_some_and(|operation| {
        operation
            .ended
            .as_ref()
            .is_some_and(|(ending, _)| result::case(*ending).is_some())
    });
    result_shown.then(|| text::open_refused(false))
}

fn build(app: &adw::Application, preview: bool) -> Rc<Ui> {
    let image_box = section_box();
    let target_box = section_box();
    let verify_box = section_box();
    // The Verify choices in the same outlined panel as the lists.
    verify_box.add_css_class("liw-options-panel");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["liw-sections"])
        .build();
    content.append(&group(&tr("Image"), &image_box));
    content.append(&group(&tr("Target"), &target_box));
    content.append(&group(&tr("Write Options"), &verify_box));

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
        .css_classes(["liw-warning"])
        .build();
    warning.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    warning.append(
        &gtk::Label::builder()
            .label(tr("All data on the target will be erased."))
            .wrap(true)
            .build(),
    );
    // Shown only while "Write" is disabled: what is still needed.
    let write_status = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "liw-write-status"])
        .visible(false)
        .build();
    // The standard primary style: its disabled look is plainly "not yet",
    // never an error. The final confirmation (next phase) carries the
    // destructive style.
    let write_button = gtk::Button::builder()
        .label(tr("Write to USB"))
        .halign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["liw-primary", "suggested-action"])
        .build();
    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["liw-write-area"])
        .build();
    bottom.append(&warning);
    bottom.append(&write_status);
    bottom.append(&write_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar());
    toolbar.set_content(Some(&scroller));
    toolbar.add_bottom_bar(&bottom);
    toolbar.set_bottom_bar_style(adw::ToolbarStyle::RaisedBorder);

    let (op, op_page) = build_operation_view();
    let views = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    views.add_named(&toolbar, Some(MAIN_VIEW));
    views.add_named(&op_page, Some(OPERATION_VIEW));
    views.set_visible_child_name(MAIN_VIEW);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&views));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Linux Image Writer")
        .default_width(600)
        .default_height(680)
        .content(&toasts)
        .build();

    let ui = Rc::new(Ui {
        window,
        views,
        toasts,
        op,
        operation: RefCell::new(None),
        image_box,
        target_box,
        verify_box,
        write_button,
        write_status,
        target_group: gtk::CheckButton::new(),
        verify_group: gtk::CheckButton::new(),
        panels: RefCell::new(Vec::new()),
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
            open_panel: None,
            preview_targets: None,
        }),
        preview,
    });

    {
        let ui_ref = ui.clone();
        ui.write_button
            .connect_clicked(move |_| start_operation(&ui_ref));
    }
    {
        let ui_ref = ui.clone();
        ui.op
            .cancel
            .connect_clicked(move |_| cancel_pressed(&ui_ref));
    }
    for (action, button) in &ui.op.actions {
        let ui_ref = ui.clone();
        let action = *action;
        button.connect_clicked(move |_| result_action(&ui_ref, action));
    }
    {
        // Closing the window would end the process and the write with it.
        let ui_ref = ui.clone();
        ui.window.connect_close_request(move |_| {
            if ui_ref.preview {
                // Nothing runs in a preview.
                glib::Propagation::Proceed
            } else if operation_running(&ui_ref) {
                // Why, in the operation's current terms (finishing or
                // stopping the write safely, or running).
                let (heading, body) = ui_ref
                    .operation
                    .borrow()
                    .as_ref()
                    .map(|operation| text::cannot_close(&operation.tracker))
                    .unwrap_or_default();
                show_cannot_close(&ui_ref, &heading, &body);
                glib::Propagation::Stop
            } else if removal_running(&ui_ref) {
                show_cannot_close(
                    &ui_ref,
                    &tr("Safely Removing the USB Drive"),
                    &tr("The window cannot be closed until it has finished. Do not unplug the USB drive yet."),
                );
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
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

// A preview does nothing real: it says so instead. Only a debug build can
// show a preview (its text, for developers, is not translated); a release
// build has no preview, so nothing to say.
fn preview_refused(ui: &Ui) {
    #[cfg(debug_assertions)]
    ui.toasts
        .add_toast(adw::Toast::new("Preview: nothing is started or written"));
    #[cfg(not(debug_assertions))]
    let _ = ui;
}

// Shows the window in a fixed development preview state (debug builds
// only; see preview.rs). Nothing real is started: the window is built with
// `preview` set, so it never lists devices, opens the file chooser, starts
// an operation or Safe Removal; the operation view draws a tracker fed
// example events, with no worker behind it. The one real call is the
// inspection of an empty example image file for the ready state (read
// only, in a temporary directory removed when the preview quits).
#[cfg(debug_assertions)]
pub fn present_preview(app: &adw::Application, request: &crate::preview::Request) {
    use crate::preview::{self, Scene, Theme};

    if let Some(theme) = request.theme {
        adw::StyleManager::default().set_color_scheme(match theme {
            Theme::Light => adw::ColorScheme::ForceLight,
            Theme::Dark => adw::ColorScheme::ForceDark,
        });
    }
    let ui = build(app, true);
    UI.with(|slot| *slot.borrow_mut() = Some(ui.clone()));

    match preview::scene(request.state) {
        Scene::Main { ready } => {
            let target = preview::example_target();
            let entry = EntryView {
                index: 0,
                title: text::device_name(&target.vendor, &target.model),
                subtitle: format!(
                    "{} · {} · {}",
                    target.device,
                    text::size(target.size),
                    text::bus(&target.connection_bus)
                ),
            };
            let protected = EntryView {
                index: 1,
                title: text::device_name("Example", "NVMe SSD"),
                subtitle: format!(
                    "nvme0n1 · {} · NVMe\n{}",
                    text::size(1_000_000_000_000),
                    text::protection(&[linux_image_writer::RiskReason::SystemDevice])
                ),
            };
            let view = TargetView {
                image_ready: ready,
                loading: false,
                failure: None,
                cleared: None,
                available: vec![entry],
                too_small: Vec::new(),
                protected: vec![protected],
                no_available: model::NoAvailableTarget::NoUsb,
                selected: ready.then_some(0),
                auto: ready,
                details: if ready {
                    vec![
                        (tr("Device"), target.device.clone()),
                        (tr("Capacity"), text::exact_bytes(target.size)),
                        (tr("Connection"), text::bus(&target.connection_bus)),
                        (tr("Removable"), tr("Yes")),
                    ]
                } else {
                    Vec::new()
                },
            };
            {
                let mut state = ui.state.borrow_mut();
                state.discovery = Discovery::Listed;
                state.preview_targets = Some(PreviewTargets {
                    view,
                    target_size: ready.then_some(target.size),
                });
            }
            if ready {
                // An empty, sparse example image: inspected like any image
                // (read only), so the main view shows a real `ImageInfo`.
                let dir = std::env::temp_dir().join(format!("liw-preview-{}", std::process::id()));
                let path = dir.join(preview::IMAGE_NAME);
                let created = std::fs::create_dir_all(&dir)
                    .and_then(|()| std::fs::File::create(&path))
                    .and_then(|file| file.set_len(1_800_000_000));
                match created {
                    Ok(()) => {
                        app.connect_shutdown(move |_| {
                            let _ = std::fs::remove_dir_all(&dir);
                        });
                        inspect(&ui, path);
                    }
                    Err(error) => eprintln!("preview: example image not created: {error}"),
                }
            }
            render_all(&ui);
        }
        Scene::Operation(scene) => {
            let scene = *scene;
            *ui.operation.borrow_mut() = Some(Operation {
                worker: None,
                tracker: scene.tracker,
                image_name: scene.image_name,
                dialogs: Vec::new(),
                ended: scene.ended,
                removal: RemovalPresentation::Unavailable,
                preview_removal: Some(RemovalPresentation::from_target(
                    scene.removal_offered.then_some(()),
                )),
            });
            ui.views.set_visible_child_name(OPERATION_VIEW);
            render_operation(&ui);
        }
    }
    ui.window.present();

    if let Some(path) = request.screenshot.clone() {
        let window = ui.window.clone();
        let app = app.clone();
        // Long enough for the first frames and the example inspection.
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            match save_screenshot(&window, &path) {
                Ok(()) => println!("preview: saved {}", path.display()),
                Err(error) => eprintln!("preview: screenshot failed: {error}"),
            }
            app.quit();
        });
    }
}

// The window as drawn, saved as a PNG: rendered by the window's own
// renderer, so it never captures anything but this window.
#[cfg(debug_assertions)]
fn save_screenshot(window: &adw::ApplicationWindow, path: &Path) -> Result<(), String> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(window.width()),
        f64::from(window.height()),
    );
    let node = snapshot.to_node().ok_or("nothing was drawn")?;
    let renderer = window.renderer().ok_or("the window has no renderer")?;
    renderer
        .render_texture(&node, None)
        .save_to_png(path)
        .map_err(|error| error.to_string())
}

// The product name, and the brand under it, small.
fn header_bar() -> adw::HeaderBar {
    adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new(
            "Linux Image Writer",
            "by PC-FREEDOM",
        ))
        .css_classes(["liw-brand"])
        .build()
}

fn section_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
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

fn details(title: &str, rows: &[(String, String)]) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().build();
    expander.set_use_markup(false);
    expander.set_title(title);
    for (name, value) in rows {
        expander.add_row(&detail_row(name, value));
    }
    expander
}

// Shows `expander` as `panel`: expanded only if it is the open panel, and
// opening it closes whichever other panel is open.
fn attach_panel(ui: &Rc<Ui>, panel: Panel, expander: &adw::ExpanderRow) {
    expander.set_expanded(expansion::is_open(ui.state.borrow().open_panel, panel));
    ui.panels.borrow_mut().push((panel, expander.clone()));
    let ui_ref = ui.clone();
    expander.connect_expanded_notify(move |expander| {
        let expanded = expander.is_expanded();
        {
            let mut state = ui_ref.state.borrow_mut();
            state.open_panel = expansion::after_toggle(state.open_panel, panel, expanded);
        }
        if expanded {
            let others: Vec<adw::ExpanderRow> = ui_ref
                .panels
                .borrow()
                .iter()
                .filter(|(other, _)| *other != panel)
                .map(|(_, row)| row.clone())
                .collect();
            for other in others {
                if other.is_expanded() {
                    other.set_expanded(false);
                }
            }
        }
    });
}

// Forgets the rows shown for `panels` before their section is redrawn.
fn detach_panels(ui: &Ui, panels: &[Panel]) {
    ui.panels
        .borrow_mut()
        .retain(|(panel, _)| !panels.contains(panel));
}

// After a redraw: a panel that is no longer shown is no longer open.
fn settle_panels(ui: &Ui, panels: &[Panel]) {
    let shown: Vec<Panel> = ui.panels.borrow().iter().map(|(panel, _)| *panel).collect();
    let mut state = ui.state.borrow_mut();
    if let Some(open) = state.open_panel
        && panels.contains(&open)
        && !shown.contains(&open)
    {
        state.open_panel = None;
    }
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
    if ui.preview {
        preview_refused(ui);
        return;
    }
    let images = gtk::FileFilter::new();
    images.set_name(Some(&tr("Disk images (ISO / IMG / GZIP / XZ)")));
    for suffix in ["iso", "img", "gz", "xz"] {
        images.add_suffix(suffix);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some(&tr("All files")));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&images);
    filters.append(&all);

    // The file chooser portal inside Flatpak; the format is decided by
    // inspection, never by these filters.
    let dialog = gtk::FileDialog::builder()
        .title(tr("Choose an Image to Write"))
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
                    message: tr("The image could not be checked"),
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
    detach_panels(ui, &[Panel::ImageDetails]);
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
                &tr("Choose an Image to Write"),
                &tr("ISO, IMG, GZIP and XZ are supported"),
            );
            let button = choose(&tr("Choose Image"));
            button.add_css_class("suggested-action");
            row.add_suffix(&button);
            rows.append(&row);
        }
        ImageState::Inspecting { name } => {
            let row = row(&tr("Checking the image…"), name);
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.update_property(&[gtk::accessible::Property::Label(&tr("Checking"))]);
            row.add_prefix(&spinner);
            row.add_suffix(&choose(&tr("Choose Another Image")));
            rows.append(&row);
        }
        ImageState::Ready { name, info, .. } => {
            // One row: the image, its summary and "Change"; the technical
            // details open inside it.
            let mut summary = format!(
                "{} · {}",
                text::size(info.file_size()),
                text::image_kind(info.compression())
            );
            if info.compression().is_some() {
                summary.push('\n');
                summary.push_str(&tr(
                    "It is decompressed while it is written to USB (the write size is checked during preparation)",
                ));
            }
            let verify = |mode| match info.verify_availability(mode) {
                VerifyAvailability::Available => "Available".to_string(),
                VerifyAvailability::Unavailable(reason) => {
                    format!("Unavailable ({})", text::verify_unavailable_name(reason))
                }
            };
            let expander = details(
                name,
                &[
                    (
                        tr("Compression"),
                        text::compression_name(info.compression()).to_string(),
                    ),
                    (tr("File size"), text::exact_bytes(info.file_size())),
                    (
                        tr("Write size"),
                        info.logical_size()
                            .map(text::exact_bytes)
                            .unwrap_or_else(|| tr("Checked during preparation (Preflight)")),
                    ),
                    (
                        "Access".to_string(),
                        text::access_name(info.access()).to_string(),
                    ),
                    ("Quick Verify".to_string(), verify(VerifyMode::Quick)),
                    ("Full Verify".to_string(), verify(VerifyMode::Full)),
                    (tr("No Verify"), verify(VerifyMode::None)),
                    (
                        tr("About Verify availability"),
                        text::verify_availability_note(),
                    ),
                    (tr("Architecture / boot method"), "Not detected".to_string()),
                ],
            );
            expander.set_subtitle(&summary);
            expander.set_subtitle_lines(0);
            let ok = gtk::Image::from_icon_name("emblem-ok-symbolic");
            let writable = tr("Can be written");
            ok.set_tooltip_text(Some(&writable));
            ok.update_property(&[gtk::accessible::Property::Label(&writable)]);
            expander.add_prefix(&ok);
            expander.add_suffix(&choose(&tr("Change")));
            expander.set_tooltip_text(Some(&tr("Open to show technical details")));
            attach_panel(ui, Panel::ImageDetails, &expander);
            rows.append(&expander);
        }
        ImageState::Failed {
            name,
            message,
            detail,
        } => {
            let expander = details(
                &tr("This image cannot be written"),
                &[(tr("Error"), detail.clone())],
            );
            expander.set_subtitle(&format!("{name} — {message}"));
            expander.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));
            expander.add_suffix(&choose(&tr("Choose Another Image")));
            attach_panel(ui, Panel::ImageDetails, &expander);
            rows.append(&expander);
        }
    }
    ui.image_box.append(&rows);
    drop(state);
    settle_panels(ui, &[Panel::ImageDetails]);
}

// ---- TARGET ----

fn refresh_targets(ui: &Rc<Ui>) {
    if ui.preview {
        return;
    }
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
    cleared: Option<String>,
    available: Vec<EntryView>,
    too_small: Vec<EntryView>,
    protected: Vec<EntryView>,
    // What to say when nothing can be chosen.
    no_available: model::NoAvailableTarget,
    selected: Option<usize>,
    auto: bool,
    details: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryView {
    index: usize,
    title: String,
    subtitle: String,
}

fn target_view(state: &State) -> TargetView {
    if let Some(preview) = &state.preview_targets {
        return preview.view.clone();
    }
    let context = image_context(state);
    let mut view = TargetView {
        image_ready: context != model::ImageContext::NotReady,
        loading: matches!(state.discovery, Discovery::Loading),
        failure: match &state.discovery {
            Discovery::Failed { detail } => Some(detail.clone()),
            _ => None,
        },
        cleared: state.choice.cleared.and_then(text::clear_reason),
        available: Vec::new(),
        too_small: Vec::new(),
        protected: Vec::new(),
        no_available: model::NoAvailableTarget::NoUsb,
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
                subtitle: format!("{place} · {}", text::shortfall(shortfall)),
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
    view.no_available = model::no_available_target(view.protected.iter().map(|entry| {
        let candidate = &state.candidates[entry.index];
        (
            candidate.display().connection_bus.as_str(),
            candidate.assessment().reasons.as_slice(),
        )
    }));
    if let Some(candidate) = state.selected.and_then(|index| state.candidates.get(index)) {
        view.details = target_details(candidate);
    }
    view
}

// The selected device's technical details, as the library reports them
// (the serial number is left out).
fn target_details(candidate: &DeviceCandidate) -> Vec<(String, String)> {
    let display = candidate.display();
    let assessment = candidate.assessment();
    let yes_no = |value: bool| if value { tr("Yes") } else { tr("No") };
    let reasons = assessment
        .reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (major, minor) = candidate.major_minor();
    vec![
        (tr("Device"), display.device.clone()),
        (tr("Vendor"), display.vendor.clone()),
        (tr("Model"), display.model.clone()),
        (tr("Capacity"), text::exact_bytes(display.size)),
        (tr("Connection"), text::bus(&display.connection_bus)),
        (tr("Removable"), yes_no(display.removable)),
        (tr("Writable"), yes_no(assessment.writable)),
        (
            "Safety".to_string(),
            format!(
                "{} ({reasons})",
                text::risk_level_name(&assessment.risk_level)
            ),
        ),
        (
            tr("Mounts"),
            if display.mount_points.is_empty() {
                tr("None")
            } else {
                display.mount_points.join(", ")
            },
        ),
        (
            "diskseq".to_string(),
            candidate
                .diskseq()
                .map(|seq| seq.to_string())
                .unwrap_or_else(|| tr("Unknown")),
        ),
        ("major:minor".to_string(), format!("{major}:{minor}")),
    ]
}

fn render_targets(ui: &Rc<Ui>, force: bool) {
    let view = target_view(&ui.state.borrow());
    if !force && ui.state.borrow().target_view.as_ref() == Some(&view) {
        return;
    }
    detach_panels(ui, &[Panel::TargetDetails, Panel::ProtectedDevices]);
    clear(&ui.target_box);

    if let Some(detail) = &view.failure {
        let rows = list();
        rows.append(&status_row(
            "dialog-error-symbolic",
            &text::candidate_list_error_message(),
            &tr("Retrying automatically in a moment"),
        ));
        rows.append(&details(
            &tr("Technical details"),
            &[(tr("Error"), detail.clone())],
        ));
        ui.target_box.append(&rows);
    }
    if let Some(cleared) = &view.cleared {
        let rows = list();
        rows.append(&status_row("dialog-warning-symbolic", cleared, ""));
        ui.target_box.append(&rows);
    }

    if view.available.is_empty() {
        let rows = list();
        if view.loading {
            let row = row(&tr("Looking for USB drives…"), "");
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.update_property(&[gtk::accessible::Property::Label(&tr("Searching"))]);
            row.add_prefix(&spinner);
            rows.append(&row);
        } else if view.too_small.is_empty() {
            let (title, line) = text::no_available_target(view.no_available);
            rows.append(&status_row("drive-removable-media-symbolic", &title, &line));
        } else {
            rows.append(&status_row(
                "drive-removable-media-symbolic",
                &tr("No USB drive is large enough for this image"),
                &tr("Connect a USB drive with more capacity"),
            ));
        }
        append_protected(ui, &rows, &view);
        ui.target_box.append(&rows);
    } else {
        if !view.image_ready {
            ui.target_box.append(&caption(&tr(
                "The target is checked once an image is chosen",
            )));
        } else if view.selected.is_none() && view.available.len() > 1 {
            ui.target_box
                .append(&caption(&tr("Choose the USB drive to write to")));
        }
        let rows = list();
        for entry in &view.available {
            let selected = view.selected == Some(entry.index);
            // The selected entry carries its details inside its own row.
            let prefix: gtk::Widget = if view.image_ready {
                let check = gtk::CheckButton::new();
                check.set_group(Some(&ui.target_group));
                check.set_active(selected);
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
                check.upcast()
            } else {
                gtk::Image::from_icon_name("drive-removable-media-symbolic").upcast()
            };
            // Before an image is ready nothing can be chosen; a choice kept
            // from an earlier image (while another one is inspected or was
            // refused) is still shown as chosen.
            let note = match (view.image_ready, selected) {
                (true, true) if view.auto => Some(tr("Selected automatically")),
                (true, _) => None,
                (false, true) => Some(tr("Selected")),
                (false, false) => Some(tr("Detected")),
            };
            if selected && !view.details.is_empty() {
                let expander = details(&entry.title, &view.details);
                expander.add_css_class("liw-selected");
                expander.set_subtitle(&entry.subtitle);
                expander.add_prefix(&prefix);
                if let Some(note) = note {
                    expander.add_suffix(&tag(&note));
                }
                expander.set_tooltip_text(Some(&tr("Open to show device details")));
                attach_panel(ui, Panel::TargetDetails, &expander);
                rows.append(&expander);
            } else {
                let row = row(&entry.title, &entry.subtitle);
                if selected {
                    row.add_css_class("liw-selected");
                }
                row.add_prefix(&prefix);
                if let Some(check) = prefix.downcast_ref::<gtk::CheckButton>() {
                    row.set_activatable_widget(Some(check));
                }
                if let Some(note) = note {
                    row.add_suffix(&tag(&note));
                }
                rows.append(&row);
            }
        }
        append_protected(ui, &rows, &view);
        ui.target_box.append(&rows);
    }

    if !view.too_small.is_empty() {
        ui.target_box.append(&caption(&tr("Too small")));
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

    ui.state.borrow_mut().target_view = Some(view);
    settle_panels(ui, &[Panel::TargetDetails, Panel::ProtectedDevices]);
}

// The protected devices, collapsed to one line at the end of the target
// list: never hidden, never selectable.
fn append_protected(ui: &Rc<Ui>, rows: &gtk::ListBox, view: &TargetView) {
    if view.protected.is_empty() {
        return;
    }
    let expander = adw::ExpanderRow::builder().build();
    expander.set_use_markup(false);
    expander.set_title(&fill(
        tr("Protected devices ({count})"),
        &[("count", &view.protected.len().to_string())],
    ));
    let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
    lock.update_property(&[gtk::accessible::Property::Label(&tr("Protected"))]);
    expander.add_prefix(&lock);
    let not_selectable = tr("Cannot be chosen as the target, for safety");
    expander.set_tooltip_text(Some(&not_selectable));
    expander.add_row(&caption_row(&not_selectable));
    for entry in &view.protected {
        expander.add_row(&row(&entry.title, &entry.subtitle));
    }
    attach_panel(ui, Panel::ProtectedDevices, &expander);
    rows.append(&expander);
}

fn caption_row(text: &str) -> gtk::ListBoxRow {
    let label = caption(text);
    label.set_margin_top(8);
    label.set_margin_bottom(8);
    label.set_margin_start(12);
    label.set_margin_end(12);
    gtk::ListBoxRow::builder()
        .child(&label)
        .activatable(false)
        .selectable(false)
        .build()
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
    ui.verify_box.append(
        &gtk::Label::builder()
            .label(tr("Verification"))
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    if info.is_none() {
        ui.verify_box.append(&caption(&tr(
            "Choose an image to set the verification mode",
        )));
    }

    // All three choices are always shown; only the selected one is
    // explained below them.
    let choices = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    for mode in VERIFY_MODES {
        let availability = info.map(|info| info.verify_availability(mode));
        let label = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        label.append(&gtk::Label::new(Some(&text::verify_title(mode))));
        let mut accessible = text::verify_title(mode);
        if mode == VerifyMode::Quick {
            label.append(
                &gtk::Label::builder()
                    .label(tr("Recommended"))
                    .valign(gtk::Align::Center)
                    .css_classes(["caption", "accent"])
                    .build(),
            );
            accessible = fill(tr("{mode} (recommended)"), &[("mode", &accessible)]);
        }
        let check = gtk::CheckButton::builder().child(&label).build();
        if let Some(VerifyAvailability::Unavailable(reason)) = availability {
            label.append(&tag(&text::verify_unavailable_short(reason)));
            check.set_tooltip_text(Some(&text::verify_unavailable(reason)));
            accessible = fill(
                tr("{label}. {reason}"),
                &[
                    ("label", &accessible),
                    ("reason", &text::verify_unavailable(reason)),
                ],
            );
        }
        check.update_property(&[gtk::accessible::Property::Label(&accessible)]);
        check.set_group(Some(&ui.verify_group));
        check.set_active(info.is_some() && state.verify.mode == mode);
        check.set_sensitive(availability == Some(VerifyAvailability::Available));
        let ui_ref = ui.clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                choose_verify(&ui_ref, mode);
            }
        });
        choices.append(&check);
    }
    ui.verify_box.append(&choices);

    if let Some(help) = text::verify_help(info.is_some(), state.verify.mode) {
        ui.verify_box.append(
            &gtk::Label::builder()
                .label(&help)
                .wrap(true)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );
    }
    if let Some(notice) = state.verify.notice {
        let line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        line.append(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        line.append(
            &gtk::Label::builder()
                .label(text::verify_notice(notice))
                .wrap(true)
                .xalign(0.0)
                .build(),
        );
        ui.verify_box.append(&line);
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
    let target_size = match &state.preview_targets {
        Some(preview) => preview.target_size,
        None => state
            .selected
            .and_then(|index| state.candidates.get(index))
            .map(|candidate| candidate.size()),
    };
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
        // An enabled button says "ready" by itself; only what is still
        // missing is written out.
        Ok(()) => {
            ui.write_button.set_sensitive(true);
            ui.write_status.set_visible(false);
        }
        Err(blocker) => {
            ui.write_button.set_sensitive(false);
            ui.write_status.set_text(&text::write_blocker(blocker));
            ui.write_status.set_visible(true);
        }
    }
}

// ---- The operation ----
//
// "Write" hands the library a request built from the current choices (the
// held `TargetRef` as the list gave it, the image path, the Verify mode) and
// starts its worker. From then on the operation decides everything: this
// view shows its messages, shows the final confirmation when -- and only
// when -- the worker asks for it, and returns the user's answer and any
// cancellation. The worker's typed outcome is what the view reports at the
// end; the device list keeps refreshing but plays no part in it.

fn operation_running(ui: &Ui) -> bool {
    ui.operation
        .borrow()
        .as_ref()
        .is_some_and(|operation| operation.ended.is_none())
}

fn start_operation(ui: &Rc<Ui>) {
    if ui.preview {
        preview_refused(ui);
        return;
    }
    if ui.operation.borrow().is_some() {
        return;
    }
    let started = {
        let state = ui.state.borrow();
        if readiness(&state).is_err() {
            return;
        }
        let (ImageState::Ready { name, path, .. }, Some(selection)) =
            (&state.image, &state.choice.selection)
        else {
            return;
        };
        // The request names the image by a UTF-8 path.
        let Some(path) = path.to_str() else {
            drop(state);
            show_message(
                ui,
                &tr("This image cannot be written"),
                &tr(
                    "The file or folder name contains characters that cannot be handled. Rename it, then choose it again.",
                ),
            );
            return;
        };
        let request = WriteOperationRequest::new(selection.held.clone(), path, state.verify.mode);
        (request, name.clone(), state.verify.mode)
    };
    let (request, image_name, verify_mode) = started;

    let worker = match spawn_write_worker(request) {
        Ok(worker) => worker,
        Err(error) => {
            show_message(
                ui,
                &tr("The Write Operation Could Not Be Started"),
                &fill(
                    tr("Nothing was written to the USB drive.\n({error})"),
                    &[("error", &error.to_string())],
                ),
            );
            return;
        }
    };
    *ui.operation.borrow_mut() = Some(Operation {
        worker: Some(worker),
        tracker: Tracker::new(verify_mode),
        image_name,
        dialogs: Vec::new(),
        ended: None,
        removal: RemovalPresentation::Unavailable,
        preview_removal: None,
    });
    ui.views.set_visible_child_name(OPERATION_VIEW);
    render_operation(ui);

    let ui_ref = ui.clone();
    glib::timeout_add_local(WORKER_POLL_INTERVAL, move || pump(&ui_ref));
}

// Reads every waiting message from the worker, then redraws once.
fn pump(ui: &Rc<Ui>) -> glib::ControlFlow {
    loop {
        let received = {
            let mut operation = ui.operation.borrow_mut();
            let Some(worker) = operation
                .as_mut()
                .and_then(|operation| operation.worker.as_mut())
            else {
                return glib::ControlFlow::Break;
            };
            worker.try_recv()
        };
        match received {
            Ok(WorkerMessage::Event(event)) => {
                if let Some(operation) = ui.operation.borrow_mut().as_mut() {
                    operation.tracker.apply(&event);
                }
            }
            Ok(WorkerMessage::ConfirmationRequested(request)) => {
                if let Some(operation) = ui.operation.borrow_mut().as_mut() {
                    operation.tracker.confirmation_requested();
                }
                show_confirmation(ui, &request);
            }
            Ok(WorkerMessage::Finished(outcome)) => {
                finish(ui, Some(*outcome));
                return glib::ControlFlow::Break;
            }
            Err(TryRecvError::Empty) => break,
            // The worker ended without an outcome.
            Err(TryRecvError::Disconnected) => {
                finish(ui, None);
                return glib::ControlFlow::Break;
            }
        }
    }
    render_operation(ui);
    glib::ControlFlow::Continue
}

// The worker sent its outcome (or was lost): nothing more comes from it.
// It is joined off the GTK thread before the ending is shown.
fn finish(ui: &Rc<Ui>, outcome: Option<OperationOutcome>) {
    let (worker, dialogs) = {
        let mut operation = ui.operation.borrow_mut();
        let Some(operation) = operation.as_mut() else {
            return;
        };
        operation.tracker.finished();
        // After `Finished` and before `join`, which consumes the worker.
        operation.removal = RemovalPresentation::from_target(
            operation
                .worker
                .as_ref()
                .and_then(WriteWorker::removal_target),
        );
        (
            operation.worker.take(),
            std::mem::take(&mut operation.dialogs),
        )
    };
    for dialog in dialogs {
        dialog.force_close();
    }
    render_operation(ui);

    let ui_ref = ui.clone();
    glib::spawn_future_local(async move {
        let joined = match worker {
            Some(worker) => gio::spawn_blocking(move || worker.join().is_ok())
                .await
                .unwrap_or(false),
            None => true,
        };
        let (ending, mut detail) = match &outcome {
            Some(outcome) => (
                operation::ending(outcome),
                operation::technical_detail(outcome),
            ),
            None => (
                Ending::Lost,
                "the worker ended without an outcome".to_string(),
            ),
        };
        if !joined {
            detail.push_str("\nthe worker thread panicked");
        }
        if let Some(operation) = ui_ref.operation.borrow_mut().as_mut() {
            operation.ended = Some((ending, detail));
        }
        if ending.returns_to_main() {
            back_to_main(
                &ui_ref,
                Some(&tr(
                    "Writing cancelled. Nothing was written to the USB drive.",
                )),
            );
        } else {
            render_operation(&ui_ref);
        }
    });
}

// A result action. The finished operation's Safe Removal state is handed
// to `result::leave`, which keeps it only while the result stays; leaving
// for the main view applies what the action keeps (`MainReturn`), then
// drops the whole operation (`back_to_main`).
fn result_action(ui: &Rc<Ui>, action: ResultAction) {
    if ui.preview {
        preview_refused(ui);
        return;
    }
    if operation_running(ui) || removal_running(ui) {
        return;
    }
    let removal = match ui.operation.borrow_mut().as_mut() {
        Some(operation) => {
            std::mem::replace(&mut operation.removal, RemovalPresentation::Unavailable)
        }
        None => return,
    };
    match result::leave(removal, action) {
        Leaving::Stay(removal) => {
            if let Some(operation) = ui.operation.borrow_mut().as_mut() {
                operation.removal = removal;
            }
            if matches!(
                action,
                ResultAction::SafeRemoval | ResultAction::RetryRemoval
            ) {
                start_removal(ui);
            }
        }
        Leaving::Main(ret) => {
            {
                let mut state = ui.state.borrow_mut();
                let choice = std::mem::take(&mut state.choice);
                let (choice, verify) = model::return_to_main(ret, choice, state.verify);
                state.choice = choice;
                state.verify = verify;
                if ret.target != TargetReturn::Keep {
                    state.selected = None;
                }
                if !ret.keep_image {
                    state.image = ImageState::Missing;
                    // An inspection still running for an earlier image is
                    // dropped.
                    state.image_generation += 1;
                }
            }
            back_to_main(ui, None);
        }
    }
}

// ---- Safe Removal ----

fn removal_running(ui: &Ui) -> bool {
    ui.operation
        .borrow()
        .as_ref()
        .is_some_and(|operation| operation.removal.is_removing())
}

// Starts Safe Removal for the finished operation's target: the state goes
// to `Removing` (taking the target, so no second request can start), the
// view shows it at once, and the library's blocking request runs on GIO's
// blocking pool -- never on the GTK thread. Its outcome, with the target,
// comes back here and becomes the state's `Finished`.
fn start_removal(ui: &Rc<Ui>) {
    let target = {
        let mut operation = ui.operation.borrow_mut();
        let Some(operation) = operation.as_mut() else {
            return;
        };
        if operation.ended.is_none() {
            return;
        }
        operation.removal.begin()
    };
    let Some(target) = target else {
        return;
    };
    render_operation(ui);

    let ui_ref = ui.clone();
    glib::spawn_future_local(async move {
        let ended = gio::spawn_blocking(move || {
            let outcome = request_safe_removal(&target);
            (target, RemovalStatus::from_outcome(&outcome))
        })
        .await;
        let dialogs = {
            let mut operation = ui_ref.operation.borrow_mut();
            let Some(operation) = operation.as_mut() else {
                return;
            };
            match ended {
                Ok((target, status)) => operation.removal.end(target, status),
                Err(_) => operation.removal.end_without_outcome(),
            }
            std::mem::take(&mut operation.dialogs)
        };
        // A "cannot close" notice shown meanwhile is over.
        for dialog in dialogs {
            dialog.force_close();
        }
        render_operation(&ui_ref);
    });
}

// Leaves the operation view. The choices made before "Write" are still
// there; the device list's next refresh applies the usual selection rules.
fn back_to_main(ui: &Rc<Ui>, toast: Option<&str>) {
    if operation_running(ui) || removal_running(ui) {
        return;
    }
    *ui.operation.borrow_mut() = None;
    ui.views.set_visible_child_name(MAIN_VIEW);
    render_all(ui);
    refresh_targets(ui);
    if let Some(toast) = toast {
        ui.toasts.add_toast(adw::Toast::new(toast));
    }
}

// ---- Final confirmation ----

// Shown only when the worker asks, with what its request says.
fn show_confirmation(ui: &Rc<Ui>, request: &WorkerConfirmationRequest) {
    let content = {
        let operation = ui.operation.borrow();
        let Some(operation) = operation.as_ref() else {
            return;
        };
        text::confirmation(
            request,
            &operation.image_name,
            operation.tracker.compression,
            operation.tracker.compressed_size,
        )
    };

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    let section = |heading: &str| {
        gtk::Label::builder()
            .label(heading)
            .xalign(0.0)
            .margin_top(8)
            .css_classes(["caption-heading", "dim-label"])
            .build()
    };
    let strong = |value: &str| {
        gtk::Label::builder()
            .label(value)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .css_classes(["heading"])
            .build()
    };
    let plain = |value: &str| {
        gtk::Label::builder()
            .label(value)
            .xalign(0.0)
            .wrap(true)
            .build()
    };
    body.append(&section(&tr("Image")));
    body.append(&strong(&content.image_name));
    for (label, value) in &content.image_lines {
        body.append(&plain(&format!("{label}: {value}")));
    }
    let arrow = gtk::Image::from_icon_name("go-down-symbolic");
    arrow.set_margin_top(4);
    arrow.update_property(&[gtk::accessible::Property::Label(&tr("To the target"))]);
    body.append(&arrow);
    body.append(&section(&tr("Target")));
    body.append(&strong(&content.target_name));
    body.append(&plain(&content.target_line));
    body.append(&section(&tr("Verification")));
    body.append(&plain(&content.verify));
    let warning = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(12)
        .build();
    warning.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    warning.append(
        &gtk::Label::builder()
            .label(&content.warning)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["warning"])
            .build(),
    );
    body.append(&warning);

    let dialog = adw::AlertDialog::new(Some(&tr("Start Writing to USB?")), None);
    dialog.set_extra_child(Some(&body));
    dialog.add_response(operation::CONFIRM_RESPONSE_CANCEL, &tr("Cancel"));
    dialog.add_response(operation::CONFIRM_RESPONSE_WRITE, &tr("Write to USB"));
    dialog.set_response_appearance(
        operation::CONFIRM_RESPONSE_WRITE,
        adw::ResponseAppearance::Destructive,
    );
    // Enter and Escape never approve.
    dialog.set_default_response(Some(operation::CONFIRM_RESPONSE_CANCEL));
    dialog.set_close_response(operation::CONFIRM_RESPONSE_CANCEL);
    let ui_ref = ui.clone();
    dialog.connect_response(None, move |dialog, response| {
        forget_dialog(&ui_ref, dialog);
        answer(&ui_ref, operation::decision_for(response));
    });
    if let Some(operation) = ui.operation.borrow_mut().as_mut() {
        operation.dialogs.push(dialog.clone());
    }
    dialog.present(Some(&ui.window));
}

// Delivers the user's answer to the pending confirmation. A decision the
// worker no longer waits for (it ended, or was cancelled) is dropped.
fn answer(ui: &Rc<Ui>, decision: ConfirmationDecision) {
    let approved = matches!(decision, ConfirmationDecision::Approved);
    {
        let mut operation = ui.operation.borrow_mut();
        let Some(operation) = operation.as_mut() else {
            return;
        };
        let Some(worker) = operation.worker.as_mut() else {
            return;
        };
        if worker.submit_confirmation(decision).is_ok() {
            operation.tracker.answered(approved);
        }
    }
    render_operation(ui);
}

fn forget_dialog(ui: &Ui, dialog: &adw::AlertDialog) {
    if let Some(operation) = ui.operation.borrow_mut().as_mut() {
        operation.dialogs.retain(|shown| shown != dialog);
    }
}

// ---- Cancel ----

fn cancel_pressed(ui: &Rc<Ui>) {
    let action = match ui.operation.borrow().as_ref() {
        Some(operation) => operation.tracker.cancel_action(),
        None => return,
    };
    match action {
        CancelAction::RequestNow => request_cancel(ui),
        CancelAction::Decline => answer(ui, ConfirmationDecision::Cancelled),
        CancelAction::AskFirst => ask_to_stop_writing(ui),
        CancelAction::Unavailable => {}
    }
}

// Asks the operation to stop at its next cancel point. What it actually
// did is its outcome, shown when it arrives.
fn request_cancel(ui: &Rc<Ui>) {
    {
        let mut operation = ui.operation.borrow_mut();
        let Some(operation) = operation.as_mut() else {
            return;
        };
        if operation.tracker.cancel_requested {
            return;
        }
        let Some(worker) = operation.worker.as_ref() else {
            return;
        };
        worker.request_cancel();
        operation.tracker.cancel_requested = true;
    }
    render_operation(ui);
}

// Once the write may have started, stopping it is confirmed once.
fn ask_to_stop_writing(ui: &Rc<Ui>) {
    let dialog = adw::AlertDialog::new(
        Some(&tr("Stop Writing?")),
        Some(&tr(
            "If you stop, the USB drive may be left with an incomplete image.",
        )),
    );
    dialog.add_response("continue", &tr("Continue Writing"));
    dialog.add_response("stop", &tr("Stop"));
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("continue"));
    dialog.set_close_response("continue");
    let ui_ref = ui.clone();
    dialog.connect_response(None, move |dialog, response| {
        forget_dialog(&ui_ref, dialog);
        if response == "stop" {
            request_cancel(&ui_ref);
        }
    });
    if let Some(operation) = ui.operation.borrow_mut().as_mut() {
        operation.dialogs.push(dialog.clone());
    }
    dialog.present(Some(&ui.window));
}

// Shown once at a time, and closed when the operation ends.
fn show_cannot_close(ui: &Rc<Ui>, heading: &str, body: &str) {
    let shown = ui.operation.borrow().as_ref().is_some_and(|operation| {
        operation
            .dialogs
            .iter()
            .any(|dialog| dialog.heading().as_deref() == Some(heading))
    });
    if shown {
        return;
    }
    let dialog = show_message(ui, heading, body);
    let ui_ref = ui.clone();
    dialog.connect_response(None, move |dialog, _| forget_dialog(&ui_ref, dialog));
    if let Some(operation) = ui.operation.borrow_mut().as_mut() {
        operation.dialogs.push(dialog);
    }
}

fn show_message(ui: &Rc<Ui>, heading: &str, body: &str) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("close", &tr("Close"));
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(Some(&ui.window));
    dialog
}

// ---- The operation view ----

// Translated where they are shown; "major:minor" and "diskseq" are
// technical names, shown as they are.
const DETAIL_NAMES: [&str; 13] = [
    n_("Image"),
    n_("Compression"),
    n_("Write size"),
    n_("Target"),
    n_("Device"),
    n_("UDisks2 object"),
    "major:minor",
    "diskseq",
    n_("Verification"),
    n_("Current stage"),
    n_("Written"),
    n_("Verified"),
    n_("Outcome"),
];

fn build_operation_view() -> (OperationUi, adw::ToolbarView) {
    let centered = |classes: &[&str]| {
        gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(classes.to_vec())
            .build()
    };
    let icon = gtk::Image::builder()
        .pixel_size(48)
        .visible(false)
        .css_classes(["liw-result-icon"])
        .build();
    // The current phase (while running) or the result (once ended).
    let title = centered(&["title-2", "liw-headline"]);
    // What the operation is doing in detail, and the safety note: under the
    // progress area, quieter than it.
    let status = centered(&["liw-status"]);
    let note = centered(&["liw-safety-note"]);

    let image = centered(&["heading"]);
    image.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    let arrow = gtk::Image::from_icon_name("go-down-symbolic");
    arrow.update_property(&[gtk::accessible::Property::Label(&tr("To the target"))]);
    let target = centered(&[]);
    let flow = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    flow.append(&image);
    flow.append(&arrow);
    flow.append(&target);

    let phases = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    // The three steps take the same width, whatever their state says.
    let columns = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    let phase_items = STEPS
        .iter()
        .enumerate()
        .map(|(index, step)| {
            if index > 0 {
                // Decoration only: the order is in the names' reading order.
                let arrow = gtk::Image::builder()
                    .icon_name("go-next-symbolic")
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label"])
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build();
                phases.append(&arrow);
            }
            let name = text::step_name(*step);
            let icon = gtk::Image::builder()
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build();
            let heading = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .halign(gtk::Align::Center)
                .build();
            heading.append(&icon);
            heading.append(
                &gtk::Label::builder()
                    .label(&name)
                    .css_classes(["heading"])
                    .build(),
            );
            let state = gtk::Label::builder()
                .wrap(true)
                .justify(gtk::Justification::Center)
                .css_classes(["caption", "dim-label"])
                .build();
            let item = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .accessible_role(gtk::AccessibleRole::Group)
                .css_classes(["liw-phase"])
                .build();
            item.append(&heading);
            item.append(&state);
            columns.add_widget(&item);
            phases.append(&item);
            PhaseItem {
                item,
                name,
                icon,
                state,
            }
        })
        .collect();

    // The progress area: how far the whole operation is (the bar, always
    // the fixed measured value, and its percentage), then that it is still
    // working (the spinner, while it runs) and the phase detail.
    let progress = gtk::ProgressBar::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .css_classes(["liw-progress"])
        .build();
    let percent = gtk::Label::builder()
        .css_classes(["liw-progress-percent", "numeric"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    let bar_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    bar_row.append(&progress);
    bar_row.append(&percent);
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .css_classes(["liw-activity"])
        .build();
    spinner.update_property(&[gtk::accessible::Property::Label(&tr("Processing"))]);
    let amount = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["liw-progress-detail", "numeric"])
        .build();
    let activity_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    activity_row.append(&spinner);
    activity_row.append(&amount);
    let progress_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .css_classes(["liw-progress-card"])
        .build();
    progress_card.append(&bar_row);
    progress_card.append(&activity_row);
    let message = centered(&["liw-result-message"]);

    let removal = {
        let list = list();
        list.set_visible(false);
        let status = row("", "");
        status.set_subtitle_lines(0);
        let spinner = gtk::Spinner::builder().spinning(true).build();
        spinner.update_property(&[gtk::accessible::Property::Label(&tr("Processing"))]);
        let icon = gtk::Image::new();
        status.add_prefix(&spinner);
        status.add_prefix(&icon);
        let extra = status_row("emblem-ok-symbolic", "", "");
        list.append(&status);
        list.append(&extra);
        RemovalUi {
            list,
            status,
            spinner,
            icon,
            extra,
        }
    };

    let details_list = list();
    let expander = adw::ExpanderRow::builder().build();
    expander.set_use_markup(false);
    expander.set_title(&tr("Technical details"));
    let details = DETAIL_NAMES
        .iter()
        .map(|name| {
            let row = detail_row(&tr(name), "");
            expander.add_row(&row);
            row
        })
        .collect();
    details_list.append(&expander);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["liw-op"])
        .build();
    // Read top to bottom: the phase (or result), how far, what it is
    // doing and the safety note; a result's message and Safe Removal; then
    // the steps and what is written where.
    for widget in [
        icon.upcast_ref::<gtk::Widget>(),
        title.upcast_ref(),
        progress_card.upcast_ref(),
        status.upcast_ref(),
        note.upcast_ref(),
        message.upcast_ref(),
        removal.list.upcast_ref(),
        phases.upcast_ref(),
        flow.upcast_ref(),
        details_list.upcast_ref(),
    ] {
        content.append(widget);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(
            &adw::Clamp::builder()
                .maximum_size(560)
                .child(&content)
                .build(),
        )
        .build();

    let cancel = gtk::Button::builder()
        .label(tr("Cancel"))
        .halign(gtk::Align::Center)
        .css_classes(["liw-action", "liw-cancel"])
        .build();
    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    bottom.append(&cancel);
    // Shown only as the result offers them (Safe Removal only for a target
    // the finished worker handed out): one centered group in one row, each
    // button as wide as its label, a small fixed gap between them.
    let action_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    bottom.append(&action_row);
    let actions = [
        ResultAction::SafeRemoval,
        ResultAction::RetryRemoval,
        ResultAction::WriteAnother,
        ResultAction::WriteAgain,
        ResultAction::Retry,
        ResultAction::BackToMain,
        ResultAction::Done,
    ]
    .into_iter()
    .map(|action| {
        let button = gtk::Button::builder()
            .label(text::result_action(action))
            .halign(gtk::Align::Center)
            .css_classes(["liw-action"])
            .visible(false)
            .build();
        action_row.append(&button);
        (action, button)
    })
    .collect();

    let page = adw::ToolbarView::new();
    page.add_top_bar(&header_bar());
    page.set_content(Some(&scroller));
    page.add_bottom_bar(&bottom);
    page.set_bottom_bar_style(adw::ToolbarStyle::RaisedBorder);

    (
        OperationUi {
            icon,
            title,
            progress_card,
            percent,
            spinner,
            status,
            note,
            image,
            target,
            phase_items,
            progress,
            amount,
            message,
            removal,
            details,
            cancel,
            actions,
            action_row,
        },
        page,
    )
}

// Shows the overall progress: the bar and its percentage at the measured
// value (never animated towards anything), the spinner while the operation
// is `working`, and the "busy" mark where the value cannot move although
// work continues (the bar's fill then breathes, as far as the system allows
// animations).
fn show_overall(op: &OperationUi, overall: OverallProgress, phase: &str, working: bool) {
    let percent = format!("{}%", overall.percent());
    op.progress.set_fraction(overall.fraction);
    op.percent.set_text(&percent);
    set_style(
        &op.progress_card,
        &["liw-busy"],
        (working && overall.busy).then_some("liw-busy"),
    );
    op.spinner.set_visible(working);
    op.spinner.set_spinning(working);
    op.progress
        .update_property(&[gtk::accessible::Property::Label(&format!(
            "{phase} {percent}"
        ))]);
    op.progress_card.set_visible(true);
}

fn render_operation(ui: &Ui) {
    let operation = ui.operation.borrow();
    let Some(operation) = operation.as_ref() else {
        return;
    };
    let op = &ui.op;
    let tracker = &operation.tracker;

    op.image.set_text(&operation.image_name);
    op.target.set_text(&match &tracker.target {
        Some(target) => text::target_summary(target),
        None => tr("Checking the target…"),
    });

    // The result view, once the outcome is joined (a declined confirmation
    // has none: the window has gone back to the main view).
    let view = operation
        .ended
        .as_ref()
        .and_then(|(ending, _)| match &operation.preview_removal {
            Some(removal) => result::view(*ending, tracker.verify_mode, removal),
            None => result::view(*ending, tracker.verify_mode, &operation.removal),
        });

    // The same row of steps while running and on the result: where each
    // step is now, or how it ended.
    let steps: [(String, &str, &str); 3] = match &view {
        None => tracker.marks().map(|mark| {
            (
                text::mark_label(mark),
                text::mark_icon(mark),
                phase_style(mark, false),
            )
        }),
        Some(view) => view.steps.map(|line| {
            (
                text::result_step(view.case, line),
                text::result_mark_icon(line.mark),
                phase_style(line.mark, true),
            )
        }),
    };
    for (phase, (state, icon, style)) in op.phase_items.iter().zip(steps) {
        phase.icon.set_icon_name(Some(icon));
        phase.state.set_text(&state);
        set_style(&phase.item, &PHASE_STYLES, Some(style));
        phase
            .item
            .update_property(&[gtk::accessible::Property::Label(&format!(
                "{}: {state}",
                phase.name
            ))]);
    }

    match &view {
        None => {
            let (status, note) = text::activity(tracker);
            let headline = text::headline(tracker);
            op.title.set_text(&headline);
            op.status.set_text(&status);
            op.status.set_visible(!status.is_empty());
            op.note.set_text(&note);
            op.note.set_visible(!note.is_empty());
            // The bar is the whole operation's progress
            // (`Tracker::overall`), always its measured value. That work
            // continues is the spinner's (not while waiting for the user's
            // confirmation); where the value cannot move although work
            // continues (finishing, stopping, waiting for the first
            // write-back confirmation) the bar is also marked busy.
            if let Some(overall) = progress::shown(tracker, None) {
                let working = tracker.activity != operation::Activity::AwaitingConfirmation;
                show_overall(op, overall, &headline, working);
            }
            let detail = text::phase_detail(tracker);
            op.amount.set_text(detail.as_deref().unwrap_or_default());
            op.amount.set_visible(detail.is_some());
            op.message.set_visible(false);
            op.icon.set_visible(false);
            op.removal.list.set_visible(false);
            let action = tracker.cancel_action();
            op.cancel.set_visible(true);
            op.cancel.set_sensitive(action != CancelAction::Unavailable);
            for (_, button) in &op.actions {
                button.set_visible(false);
            }
            op.action_row.set_visible(false);
        }
        Some(view) => {
            op.icon.set_icon_name(Some(text::result_icon(view.kind)));
            set_style(&op.icon, &SEMANTIC_STYLES, result_style(view.kind));
            op.icon.set_visible(true);
            op.title.set_text(&text::result_title(view.case));
            op.status.set_visible(false);
            op.note.set_visible(false);
            // 100% only for a completed operation; a cancelled or failed
            // one shows no progress at all.
            match progress::shown(tracker, operation.ended.as_ref().map(|(ending, _)| *ending)) {
                Some(overall) => show_overall(op, overall, &text::result_title(view.case), false),
                None => op.progress_card.set_visible(false),
            }
            op.amount.set_visible(false);
            op.message.set_text(&text::result_message(view.case));
            op.message.set_visible(true);
            op.cancel.set_visible(false);
            render_removal(
                &op.removal,
                view.removal,
                tracker
                    .target
                    .as_ref()
                    .map(|target| text::device_name(&target.vendor, &target.model))
                    .as_deref(),
            );
            // The first action shown is the suggested one; none can be
            // pressed while Safe Removal runs.
            let mut first = true;
            for (action, button) in &op.actions {
                let offered = view.actions.contains(action);
                button.set_visible(offered);
                button.set_sensitive(view.actions_enabled);
                if offered && first {
                    button.add_css_class("suggested-action");
                    first = false;
                } else {
                    button.remove_css_class("suggested-action");
                }
            }
            op.action_row.set_visible(!view.actions.is_empty());
        }
    }
    // Before the outcome is joined and shown, neither button acts.
    if view.is_none() && tracker.activity == operation::Activity::Finished {
        op.cancel.set_sensitive(false);
    }

    let pending = tr("Checking");
    let values = [
        operation.image_name.clone(),
        match tracker.compression {
            Some(format) => text::compression_name(Some(format)).to_string(),
            None if tracker.image_size.is_some() => text::compression_name(None).to_string(),
            None => pending.clone(),
        },
        tracker
            .image_size
            .map(text::exact_bytes)
            .unwrap_or_else(|| pending.clone()),
        tracker
            .target
            .as_ref()
            .map(|target| text::device_name(&target.vendor, &target.model))
            .unwrap_or_else(|| pending.clone()),
        tracker
            .target
            .as_ref()
            .map(|target| target.device.clone())
            .unwrap_or_else(|| pending.clone()),
        tracker
            .block_path
            .clone()
            .unwrap_or_else(|| pending.clone()),
        tracker
            .major_minor
            .map(|(major, minor)| format!("{major}:{minor}"))
            .unwrap_or_else(|| tr("Not available")),
        tracker
            .diskseq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| tr("Unknown")),
        text::verify_title(tracker.verify_mode),
        format!("{:?}", tracker.activity),
        tracker
            .written
            .map(|written| text::exact_bytes(written.done))
            .unwrap_or_else(|| text::exact_bytes(0)),
        tracker
            .verified
            .map(|verified| text::exact_bytes(verified.done))
            .unwrap_or_else(|| text::exact_bytes(0)),
        operation
            .ended
            .as_ref()
            .map(|(_, detail)| detail.clone())
            .unwrap_or_else(|| tr("Running")),
    ];
    for (row, value) in op.details.iter().zip(values) {
        row.set_subtitle(&value);
    }
}

// The Safe Removal section, from the result view's notice. `target_name` is
// the target as the operation's own events named it.
fn render_removal(
    ui: &RemovalUi,
    notice: Option<result::RemovalNotice>,
    target_name: Option<&str>,
) {
    let Some(notice) = notice else {
        ui.list.set_visible(false);
        return;
    };
    let words = text::removal(notice, target_name);
    ui.status.set_title(&words.title);
    ui.status.set_subtitle(&words.message);
    let removed = matches!(
        notice,
        RemovalNotice::Finished(RemovalStatus::Removed { .. })
    );
    set_style(
        &ui.status,
        &["liw-removed"],
        removed.then_some("liw-removed"),
    );
    set_style(&ui.icon, &SEMANTIC_STYLES, removal_style(notice));
    match text::removal_icon(notice) {
        Some(icon) => {
            ui.icon.set_icon_name(Some(icon));
            ui.icon.set_visible(true);
            ui.spinner.set_visible(false);
        }
        None => {
            ui.icon.set_visible(false);
            ui.spinner.set_visible(true);
        }
    }
    match words.extra {
        Some(extra) => {
            ui.extra.set_title(&extra);
            ui.extra.set_visible(true);
        }
        None => ui.extra.set_visible(false),
    }
    ui.list.set_visible(true);
}

// ---- Style classes (style.css) ----
//
// Colour only repeats what the words and icons already say.

const PHASE_STYLES: [&str; 5] = [
    "liw-phase-active",
    "liw-phase-done",
    "liw-phase-failed",
    "liw-phase-cancelled",
    "liw-phase-idle",
];

const SEMANTIC_STYLES: [&str; 3] = ["success", "warning", "error"];

// How a step looks: the current one, how it ended, or low emphasis (still to
// come, not requested, or -- once the operation `ended` -- not run).
fn phase_style(mark: Mark, ended: bool) -> &'static str {
    match mark {
        Mark::Active if !ended => "liw-phase-active",
        Mark::Done => "liw-phase-done",
        Mark::Failed => "liw-phase-failed",
        Mark::Cancelled => "liw-phase-cancelled",
        Mark::Waiting | Mark::Active | Mark::Skipped => "liw-phase-idle",
    }
}

// The result icon's colour: written but not verified stays neutral.
fn result_style(kind: ResultKind) -> Option<&'static str> {
    match kind {
        ResultKind::Success => Some("success"),
        ResultKind::WrittenNotVerified => None,
        ResultKind::Cancelled => Some("warning"),
        ResultKind::Failed => Some("error"),
    }
}

// The Safe Removal icon's colour, alongside `text::removal_icon`.
fn removal_style(notice: RemovalNotice) -> Option<&'static str> {
    match notice {
        RemovalNotice::Removing => None,
        RemovalNotice::Finished(status) => match status {
            RemovalStatus::Removed { .. } => Some("success"),
            RemovalStatus::DeviceGone | RemovalStatus::Unsupported => None,
            RemovalStatus::DeviceChanged
            | RemovalStatus::Busy
            | RemovalStatus::NotAuthorized
            | RemovalStatus::NotCompleted => Some("warning"),
        },
    }
}

// Leaves exactly `class` (if any) of `classes` on `widget`.
fn set_style(widget: &impl IsA<gtk::Widget>, classes: &[&str], class: Option<&str>) {
    for other in classes {
        if Some(*other) != class {
            widget.remove_css_class(other);
        }
    }
    if let Some(class) = class {
        widget.add_css_class(class);
    }
}

#[cfg(test)]
mod tests {
    // Every app-specific style class the window sets (`liw-…`) is defined
    // in the stylesheet: the look lives in style.css, not in the code, and
    // a class with no rule is a typo.
    #[test]
    fn every_style_class_is_defined() {
        let code = include_str!("window.rs");
        let code = &code[..code.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        let css = include_str!("style.css");
        let mut classes: Vec<&str> = code
            .split('"')
            .skip(1)
            .step_by(2)
            // Class names only (not, say, the preview's `liw-preview-{pid}` directory).
            .filter(|literal| {
                literal.starts_with("liw-")
                    && literal.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    && !literal.ends_with('-')
            })
            .collect();
        classes.sort();
        classes.dedup();
        assert!(classes.len() >= 15, "{classes:?}");
        for class in classes {
            assert!(css.contains(&format!(".{class}")), "no rule for {class}");
        }
    }
}
