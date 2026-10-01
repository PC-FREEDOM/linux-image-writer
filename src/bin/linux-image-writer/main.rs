// Linux Image Writer's GUI (GTK4 + libadwaita).
//
// A consumer of the library's Production API only: the device list and its
// Safety Engine verdicts (`list_candidates`), the identity check across
// refreshes (`DeviceCandidate::is_same_device_as`), image inspection
// (`inspect_image`) and its Verify rule (`ImageInfo::verify_availability`).
// Nothing here decides Safety, identity, format or Verify support.
//
// Choosing the image, the target and the Verify mode (Phase 3B-1b-1), and
// running the write through the library's worker (`spawn_write_worker`,
// Phase 3B-1b-2): the operation selects, prepares, asks for the final
// confirmation, writes, syncs and verifies on its own thread; this program
// shows its messages and returns the user's answers (`Approved` or
// `Cancelled`, and cancellation). Nothing here opens a device.
//
// The result view (Phase 3B-1b-3a) reads the worker's outcome and whether
// it handed out a Safe Removal target (`WriteWorker::removal_target`). For
// that target only, "Safely remove" asks the library to do it
// (`request_safe_removal`, Phase 3B-1b-3b, off the GTK thread), which
// re-checks, unmounts and powers off by itself; this program shows its
// outcome.
//
//   linux-image-writer [image]   (an image given here is inspected at start)

mod expansion;
mod i18n;
mod model;
mod operation;
mod result;
mod text;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

// The canonical application ID (GitHub account PC-FREEDOM, the hyphen
// written as an underscore). The desktop entry, AppStream metainfo, icon
// name and Flatpak manifest use the same ID.
const APP_ID: &str = "io.github.pc_freedom.linux-image-writer";

fn main() -> glib::ExitCode {
    i18n::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|_| load_style());
    app.connect_activate(|app| window::present(app, None));
    app.connect_open(|app, files, _| {
        window::present(app, files.first().and_then(|file| file.path()));
    });
    app.run()
}

// The app's own look (style.css), over libadwaita's, and its dark palette
// (style-dark.css) while the style is dark. Both are compiled in. Presentation
// only: no widget depends on them to work or to be understood.
fn load_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = |css: &str| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(css);
        provider
    };
    gtk::style_context_add_provider_for_display(
        &display,
        &provider(include_str!("style.css")),
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let dark = provider(include_str!("style-dark.css"));
    let apply = move |manager: &adw::StyleManager| {
        if manager.is_dark() {
            gtk::style_context_add_provider_for_display(
                &display,
                &dark,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        } else {
            gtk::style_context_remove_provider_for_display(&display, &dark);
        }
    };
    let manager = adw::StyleManager::default();
    apply(&manager);
    manager.connect_dark_notify(apply);
}
