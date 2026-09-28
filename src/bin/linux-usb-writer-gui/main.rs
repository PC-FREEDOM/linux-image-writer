// Linux USB Writer's GUI (GTK4 + libadwaita).
//
// A consumer of the library's Production API only: the device list and its
// Safety Engine verdicts (`list_candidates`), the identity check across
// refreshes (`DeviceCandidate::is_same_device_as`), image inspection
// (`inspect_image`) and its Verify rule (`ImageInfo::verify_availability`).
// Nothing here decides Safety, identity, format or Verify support.
//
// Phase 3B-1b-1: choosing the image, the target and the Verify mode. The
// write operation is not connected yet -- nothing in this program opens a
// device or writes anything.
//
//   linux-usb-writer-gui [image]   (an image given here is inspected at start)

mod model;
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
