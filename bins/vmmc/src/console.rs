//! A running VM's console in the app: its shared screen, and a thread that sends input
//! and commands down its pipe, so a slow worker never stalls the window.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use ipc::Request;
use ipc::pipe::Section;
use ipc::screen::{Cursor, Picture};
use vmconfig::Vm;

pub struct Session {
    /// The VM's file.
    pub path: PathBuf,
    requests: Sender<Request>,
    section: Section,
    frame_seq: u32,
    cursor_seq: u32,
    /// The size of the last picture.
    pub frame: (u32, u32),
    /// The size last asked of the guest.
    pub asked: Option<(u32, u32)>,
}

impl Session {
    /// Connects to the running VM's worker and maps its screen.
    pub fn open(vm: &Vm) -> Result<Self, String> {
        let mut connection = vmmc_client::connect(vm, Duration::from_millis(500))?;
        let section = connection.screen()?;
        let (requests, queued) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("console-requests".into())
            .spawn(move || {
                for request in queued {
                    if let Err(e) = connection.call(&request) {
                        tracing::debug!(%e, "the VM's worker went away");
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: vm.path.clone(),
            requests,
            section,
            frame_seq: 0,
            cursor_seq: 0,
            frame: (1, 1),
            asked: None,
        })
    }

    /// Sends a request; false once the worker is gone.
    pub fn send(&self, request: Request) -> bool {
        self.requests.send(request).is_ok()
    }

    /// Where keys go, for the keyboard hook.
    pub fn sender(&self) -> Sender<Request> {
        self.requests.clone()
    }

    /// A new picture, if the guest drew one.
    pub fn picture(&mut self) -> Option<Picture> {
        let picture = self.section.view().read_picture(&mut self.frame_seq)?;
        self.frame = (picture.width, picture.height);
        Some(picture)
    }

    /// The guest's cursor, if it changed.
    pub fn cursor(&mut self) -> Option<Cursor> {
        self.section.view().read_cursor(&mut self.cursor_seq)
    }
}

/// Where `(x, y)` in an `area`-sized view lands on the tablet, when a `frame`-sized
/// picture is scaled to fit the view and centred (Slint's `contain`).
pub fn tablet_position(x: f32, y: f32, area: (f32, f32), frame: (u32, u32)) -> (u32, u32) {
    const TABLET_MAX: f32 = 32767.0;
    let (frame_w, frame_h) = (frame.0.max(1) as f32, frame.1.max(1) as f32);
    let scale = (area.0 / frame_w)
        .min(area.1 / frame_h)
        .max(f32::MIN_POSITIVE);
    let (shown_w, shown_h) = (frame_w * scale, frame_h * scale);
    let (left, top) = ((area.0 - shown_w) / 2.0, (area.1 - shown_h) / 2.0);
    let fraction = |value: f32, start: f32, size: f32| ((value - start) / size).clamp(0.0, 1.0);
    (
        (fraction(x, left, shown_w) * TABLET_MAX).round() as u32,
        (fraction(y, top, shown_h) * TABLET_MAX).round() as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_maps_onto_the_letterboxed_picture() {
        // A 4:3 picture in a wide view: bars left and right.
        let area = (2000.0, 750.0);
        let frame = (1024, 768);
        assert_eq!(tablet_position(500.0, 0.0, area, frame), (0, 0));
        assert_eq!(tablet_position(1000.0, 375.0, area, frame), (16384, 16384));
        assert_eq!(tablet_position(1999.0, 749.0, area, frame), (32767, 32723));
        assert_eq!(tablet_position(10.0, 10.0, area, frame).0, 0, "on the bar");
    }
}
