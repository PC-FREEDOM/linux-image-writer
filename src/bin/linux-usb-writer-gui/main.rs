// Linux USB Writer's GUI (GTK4 + libadwaita).
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
//   linux-usb-writer-gui [image]   (an image given here is inspected at start)

mod expansion;
mod model;
mod operation;
mod result;
mod text;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

// TODO: the formal application ID is decided with the packaging work.
const APP_ID: &str = "io.github.pcfreedom.LinuxUsbWriter";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_activate(|app| window::present(app, None));
    app.connect_open(|app, files, _| {
        window::present(app, files.first().and_then(|file| file.path()));
    });
    app.run()
}
