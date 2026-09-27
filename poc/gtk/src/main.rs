// Phase 3B-0A PoC: a GTK4 + libadwaita window that lists the devices the
// Production library sees. It uses the read-only `list_candidates()` only:
// no write request, no worker, no device open. The serial number is never
// shown or printed.
//
//   gtk-poc           the window (also logs the display backend and the
//                     color scheme to stdout, and can pick an image file
//                     through the file chooser portal)
//   gtk-poc --probe   the same discovery plus the sandbox checks, as text
//                     (no display needed; used inside the Flatpak sandbox)
//   gtk-poc --source-identity <path> <seconds>
//                     open the file, take the metadata the Production
//                     Source Identity compares (fstat of the open file: dev,
//                     ino, size, mtime, ctime), wait, take it again from the
//                     same open file and compare. Reads nothing else.
//   gtk-poc --source-identity-poll <path> <seconds>
//                     the same metadata from the same open file every 50 ms,
//                     printing when (Unix time) it first differs: how soon a
//                     change made elsewhere becomes visible through fstat.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use linux_usb_writer::{DeviceCandidate, NotSelectableReason, Selectability, list_candidates};
use std::os::unix::fs::MetadataExt;

const APP_ID: &str = "io.github.pcfreedom.LinuxUsbWriter.GtkPoc";

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--probe") => {
            probe();
            return glib::ExitCode::SUCCESS;
        }
        Some("--source-identity-poll") => {
            let seconds = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            return match args.get(2) {
                Some(path) => source_identity_poll(path, seconds),
                None => glib::ExitCode::FAILURE,
            };
        }
        Some("--source-identity") => {
            let seconds = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            return match args.get(2) {
                Some(path) => source_identity_check(path, seconds),
                None => glib::ExitCode::FAILURE,
            };
        }
        _ => {}
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run_with_args(&[] as &[&str])
}

fn build_window(app: &adw::Application) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    content.append(&label(&format!(
        "GTK4: OK ({}.{}.{})   libadwaita: OK ({}.{}.{})",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version(),
    )));
    content.append(&label(&sandbox_summary()));

    let style = label("");
    content.append(&style);

    let choose = gtk::Button::with_label("Choose image…");
    choose.set_halign(gtk::Align::Start);
    content.append(&choose);
    let chosen = label("No image chosen.");
    content.append(&chosen);

    let refresh = gtk::Button::with_label("Refresh devices");
    refresh.add_css_class("suggested-action");
    refresh.set_halign(gtk::Align::Start);
    content.append(&refresh);

    let status = label("Press Refresh devices.");
    content.append(&status);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scroller);

    let refresh_for_start = refresh.clone();
    refresh.connect_clicked(move |button| {
        button.set_sensitive(false);
        status.set_text("Listing devices…");
        let (button, status, list) = (button.clone(), status.clone(), list.clone());
        // `list_candidates()` blocks on D-Bus: run it off the GTK thread and
        // come back to it with the result.
        glib::spawn_future_local(async move {
            let result = gtk::gio::spawn_blocking(list_candidates).await;
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            match result {
                Ok(Ok(candidates)) => {
                    status.set_text(&format!("Detected devices: {}", candidates.len()));
                    for candidate in &candidates {
                        list.append(&candidate_row(candidate));
                    }
                }
                Ok(Err(error)) => status.set_text(&format!("Device list failed: {error:?}")),
                Err(_) => status.set_text("Device list failed: the lister panicked"),
            }
            button.set_sensitive(true);
        });
    });

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Linux USB Writer — GTK4 / libadwaita Distribution PoC")
        .default_width(760)
        .default_height(520)
        .content(&toolbar)
        .build();
    let window_for_dialog = window.clone();
    window.present();
    // List once on start, through the same button callback.
    refresh_for_start.emit_clicked();

    // Display backend and color scheme, shown and logged now and whenever
    // the desktop's scheme changes.
    let manager = adw::StyleManager::default();
    let show_style = move |manager: &adw::StyleManager| {
        let text = style_summary(manager);
        println!("style: {text}");
        style.set_text(&text);
    };
    show_style(&manager);
    manager.connect_dark_notify(move |manager| show_style(manager));

    // The file chooser: inside Flatpak this is the portal, and a file from
    // outside the sandbox comes back as a document portal path.
    choose.connect_clicked(move |_| {
        let chosen = chosen.clone();
        let window_for_dialog = window_for_dialog.clone();
        gtk::FileDialog::builder()
            .title("Choose an image")
            .build()
            .open(
                Some(&window_for_dialog),
                None::<&gio::Cancellable>,
                move |result| {
                    let text = match result.map(|file| file.path()) {
                        Ok(Some(path)) => {
                            let path = path.display().to_string();
                            match std::fs::File::open(&path).and_then(|file| file.metadata()) {
                                Ok(meta) => format!("{path}\n{}", identity_text(&meta)),
                                Err(error) => format!("{path}\nopen failed: {error}"),
                            }
                        }
                        Ok(None) => "The chosen file has no local path.".to_string(),
                        Err(error) => format!("No file: {error}"),
                    };
                    println!("chosen: {}", text.replace('\n', " | "));
                    chosen.set_text(&text);
                },
            );
    });
}

fn style_summary(manager: &adw::StyleManager) -> String {
    let backend = gdk::Display::default()
        .map(|display| display.type_().name().to_string())
        .unwrap_or_else(|| "no display".to_string());
    format!(
        "display: {backend}   system color schemes: {}   dark: {}",
        if manager.system_supports_color_schemes() {
            "supported (portal)"
        } else {
            "not supported"
        },
        manager.is_dark()
    )
}

// The fields the Production Source Identity compares, from an open file.
fn identity_text(meta: &std::fs::Metadata) -> String {
    format!(
        "dev={} ino={} size={} mtime={}.{:09} ctime={}.{:09}",
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec()
    )
}

fn source_identity_check(path: &str, seconds: f64) -> glib::ExitCode {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            println!("open failed: {error}");
            return glib::ExitCode::FAILURE;
        }
    };
    let identity = |file: &std::fs::File| file.metadata().map(|meta| identity_text(&meta));
    let (Ok(first), fd_target) = (
        identity(&file),
        std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&file)
        ))
        .map(|t| t.display().to_string()),
    ) else {
        println!("fstat failed");
        return glib::ExitCode::FAILURE;
    };
    println!("path:   {path}");
    println!("fd:     {fd_target:?}");
    println!("first:  {first}");
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    match identity(&file) {
        Ok(second) => {
            println!("second: {second}");
            println!(
                "result: {}",
                if first == second {
                    "UNCHANGED"
                } else {
                    "CHANGED"
                }
            );
        }
        Err(error) => println!("second fstat failed: {error}"),
    }
    glib::ExitCode::SUCCESS
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true);
    label
}

fn candidate_row(candidate: &DeviceCandidate) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&describe_title(candidate)))
        .subtitle(glib::markup_escape_text(&describe_details(candidate)))
        .build();
    row.set_subtitle_lines(3);
    row
}

fn describe_title(candidate: &DeviceCandidate) -> String {
    let display = candidate.display();
    format!(
        "{}  {} {}  ({})",
        display.device,
        display.vendor,
        display.model,
        human_size(display.size)
    )
}

fn describe_details(candidate: &DeviceCandidate) -> String {
    let display = candidate.display();
    let assessment = candidate.assessment();
    let selectable = match candidate.selectability() {
        Selectability::Selectable => "selectable".to_string(),
        Selectability::NotSelectable(reasons) => format!(
            "not selectable: {}",
            reasons
                .iter()
                .map(not_selectable_text)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    format!(
        "bus: {}   removable: {}   {}\nrisk: {:?}   writable: {}   reasons: {:?}",
        if display.connection_bus.is_empty() {
            "-"
        } else {
            &display.connection_bus
        },
        display.removable,
        selectable,
        assessment.risk_level,
        assessment.writable,
        assessment.reasons,
    )
}

fn not_selectable_text(reason: &NotSelectableReason) -> &'static str {
    match reason {
        NotSelectableReason::NotWritable => "not writable",
        NotSelectableReason::RiskNotNormal => "risk not Normal",
        NotSelectableReason::MediaUnavailable => "no media",
        NotSelectableReason::ZeroSize => "size 0",
    }
}

fn human_size(bytes: u64) -> String {
    let gb = bytes as f64 / 1_000_000_000.0;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

// What the device list depends on besides the UDisks2 system bus, checked
// directly (the Production backend reads the same files).
fn sandbox_summary() -> String {
    let flatpak = std::path::Path::new("/.flatpak-info").exists();
    let swaps = match std::fs::read_to_string("/proc/swaps") {
        Ok(contents) => format!("readable ({} active)", contents.lines().skip(1).count()),
        Err(error) => format!("NOT readable ({error})"),
    };
    let diskseq = diskseq_check();
    format!(
        "Flatpak sandbox: {}   /proc/swaps: {}   /sys/block/*/diskseq: {}",
        if flatpak { "yes" } else { "no" },
        swaps,
        diskseq
    )
}

fn diskseq_check() -> String {
    let entries = match std::fs::read_dir("/sys/block") {
        Ok(entries) => entries,
        Err(error) => return format!("/sys/block NOT readable ({error})"),
    };
    let (mut read, mut failed) = (0, 0);
    for entry in entries.flatten() {
        match std::fs::read_to_string(entry.path().join("diskseq")) {
            Ok(value) if value.trim().parse::<u64>().is_ok() => read += 1,
            _ => failed += 1,
        }
    }
    format!("{read} readable, {failed} not")
}

fn probe() {
    println!("{}", sandbox_summary());
    match list_candidates() {
        Ok(candidates) => {
            println!("list_candidates: OK, {} devices", candidates.len());
            for candidate in &candidates {
                println!("  {}", describe_title(candidate));
                for line in describe_details(candidate).lines() {
                    println!("    {line}");
                }
            }
        }
        Err(error) => println!("list_candidates: FAILED: {error:?}"),
    }
}

fn source_identity_poll(path: &str, seconds: f64) -> glib::ExitCode {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    };
    let Ok(file) = std::fs::File::open(path) else {
        println!("open failed");
        return glib::ExitCode::FAILURE;
    };
    let identity = |file: &std::fs::File| file.metadata().map(|meta| identity_text(&meta)).ok();
    let first = identity(&file);
    println!("ready at {:.3}", now());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
    while std::time::Instant::now() < deadline {
        if identity(&file) != first {
            println!("changed seen at {:.3}", now());
            return glib::ExitCode::SUCCESS;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("no change seen");
    glib::ExitCode::SUCCESS
}
