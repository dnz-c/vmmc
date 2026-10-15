//! Renders the window offscreen in each of its states (library, a VM's page, the
//! console, the wizard, settings, a message) with Slint's software renderer, and
//! saves them in `target/ui-snapshots`, for looking at. It fails if a state draws
//! nothing.

use std::path::PathBuf;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, VecModel};

use super::{AppWindow, TabRow, VmRow};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 820;

struct Offscreen(Rc<MinimalSoftwareWindow>);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn snapshot(window: &MinimalSoftwareWindow, name: &str) {
    let mut buffer = vec![Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, WIDTH as usize);
    });
    let first = buffer[0];
    assert!(
        buffer.iter().any(|pixel| *pixel != first),
        "{name}: nothing drawn"
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = buffer.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&bytes)
        .unwrap();
}

fn vm(name: &str, running: bool) -> VmRow {
    VmRow {
        path: format!(r"C:\VMs\{name}\{name}.vmmc").into(),
        name: name.into(),
        folder: "".into(),
        heading: "".into(),
        os: "Windows 11".into(),
        running,
        summary: "4 vCPU, 8.0 GiB, Nvme".into(),
        details: "4 processors, 8.0 GiB memory\nNVMe disk: Windows 11.vmdk\nDVD drive: empty\nNetwork: e1000e on NAT, 52:54:00:4e:5f:60\nUEFI firmware, Secure Boot on, TPM 2.0 on".into(),
        notes: "My test machine.".into(),
        created: "2026-10-15T09:00:00Z".into(),
        started: "2026-10-15T10:30:00Z".into(),
        origin: "".into(),
    }
}

#[test]
fn every_state_draws() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    window.set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));
    app.show().unwrap();

    snapshot(&window, "empty");

    let rows = vec![vm("Windows 11", false), vm("Ubuntu", true)];
    app.set_vms(ModelRc::new(VecModel::from(rows.clone())));
    app.set_tabs(ModelRc::new(VecModel::from(vec![
        TabRow {
            path: rows[0].path.clone(),
            name: rows[0].name.clone(),
            running: false,
        },
        TabRow {
            path: rows[1].path.clone(),
            name: rows[1].name.clone(),
            running: true,
        },
    ])));
    app.set_current_tab(0);
    app.set_current(rows[0].clone());
    snapshot(&window, "vm-page");

    app.set_current_tab(1);
    app.set_current(rows[1].clone());
    app.set_console_live(true);
    let mut picture = SharedPixelBuffer::<Rgb8Pixel>::new(1024, 768);
    for (index, pixel) in picture.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (index % 1024, index / 1024);
        *pixel = Rgb8Pixel {
            r: (x / 4) as u8,
            g: (y / 3) as u8,
            b: 120,
        };
    }
    app.set_frame_width(1024);
    app.set_frame_height(768);
    app.set_screen(Image::from_rgb8(picture));
    app.set_keyboard_captured(true);
    snapshot(&window, "console");
    app.set_fullscreen(true);
    app.set_fullscreen_bar(true);
    snapshot(&window, "fullscreen");
    app.set_fullscreen(false);
    app.set_fullscreen_bar(false);

    app.set_os_labels(super::strings(
        vmconfig::presets::PRESETS.iter().map(|p| p.label),
    ));
    app.set_editions(super::strings(super::media_editions()));
    app.set_wizard_name("Windows 11".into());
    app.set_wizard_os(0);
    app.set_wizard_iso(r"C:\ISOs\Win11_24H2.iso".into());
    app.set_wizard_easy(true);
    app.set_wizard_user("anna".into());
    app.set_wizard_password("secret".into());
    app.set_wizard_cpus(4);
    app.set_wizard_memory_gib(8);
    app.set_wizard_disk_gib(64);
    app.set_wizard_folder(r"C:\Users\anna\Documents\vmmc VMs".into());
    app.set_wizard_error("Easy Install needs a password for that user".into());
    app.set_wizard_open(true);
    snapshot(&window, "wizard");
    app.set_wizard_open(false);

    app.set_current_tab(0);
    app.set_current(rows[0].clone());
    app.set_settings_cpus(4);
    app.set_settings_memory_mib(8192);
    app.set_settings_cdrom(r"C:\ISOs\virtio.iso".into());
    app.set_settings_secure_boot(true);
    app.set_settings_tpm(true);
    app.set_settings_enlightenments(true);
    app.set_settings_open(true);
    snapshot(&window, "settings");
    app.set_settings_open(false);

    app.set_message_title("Imported".into());
    app.set_message_text("Windows 10 x64 opens in place.\n\nWorth knowing:\n• no sound card yet\n• Windows sees new hardware: it may ask to be activated again".into());
    app.set_message_open(true);
    snapshot(&window, "message");
}
