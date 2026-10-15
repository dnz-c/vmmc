//! Easy Install's first boot: the VM's config lists what to type on the serial console
//! when (`[install]`): Windows' "Press any key to boot from CD or DVD", Ubuntu's GRUB
//! command line. A thread watches the guest's serial output (as plain text) for each
//! step's text and types its keys. Once the last is typed, the steps leave the config,
//! so later boots don't install again; when the install is over (a power-off or a reset,
//! as the config says), the install discs come out and the guest starts from its disk.

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use vmconfig::{EjectOn, Vm};

use crate::console::Targets;

/// How much recent output is searched for a step's text.
const WINDOW: usize = 16 << 10;

struct State {
    vm: Vm,
    /// The next step to wait for.
    next: usize,
}

impl State {
    fn done(&self) -> bool {
        self.vm
            .config
            .install
            .as_ref()
            .is_none_or(|install| self.next >= install.steps.len())
    }
}

/// The watcher of one VM's install.
pub struct Installer {
    state: Arc<Mutex<State>>,
    output: Sender<Vec<u8>>,
}

impl Installer {
    /// Starts watching, if the VM has an install to do.
    pub fn start(vm: &Vm, targets: Targets) -> Option<Self> {
        let install = vm.config.install.as_ref()?;
        let state = Arc::new(Mutex::new(State {
            vm: vm.clone(),
            next: 0,
        }));
        let (output, received) = mpsc::channel();
        if !install.steps.is_empty() {
            eprint!(
                "\r\nvmmc-vmx: Easy Install: {} steps on the serial console\r\n",
                install.steps.len()
            );
            let watched = state.clone();
            thread::Builder::new()
                .name("easy-install".into())
                .spawn(move || watch(&watched, &received, &targets))
                .ok()?;
        }
        Some(Self { state, output })
    }

    /// The guest's serial output, passed on to `console` and to the watcher.
    pub fn tap(&self, console: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
        Box::new(Tap {
            console,
            watcher: self.output.clone(),
        })
    }

    /// The machine stopped for `outcome`. If that ends the install, the install discs
    /// come out of the config (and the answer disc is deleted): the VM, changed, to
    /// start again from its disk.
    pub fn finish(&self, outcome: &machine::RunOutcome) -> Option<Vm> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let install = state.vm.config.install.clone()?;
        let ends = matches!(
            (install.eject_on, outcome),
            (Some(EjectOn::PowerOff), machine::RunOutcome::PowerOff)
                | (Some(EjectOn::Reset), machine::RunOutcome::Reset)
        );
        if !ends || !state.done() {
            return None;
        }
        let vm = &mut state.vm;
        for cdrom in vm.config.cdroms.iter().filter(|cd| cd.install_media) {
            if cdrom.path.as_deref() == Some(vmconfig::create::ANSWER_DISC) {
                let _ = std::fs::remove_file(vm.resolve(vmconfig::create::ANSWER_DISC));
            }
        }
        // The first drive stays, empty, for a disc later; the answer disc's goes.
        let mut kept = false;
        vm.config.cdroms.retain_mut(|cd| {
            if !cd.install_media {
                return true;
            }
            if kept {
                return false;
            }
            kept = true;
            cd.path = None;
            cd.install_media = false;
            true
        });
        vm.config.install = None;
        if let Err(e) = vm.save() {
            eprint!("\r\nvmmc-vmx: Easy Install: can't update the VM's file: {e}\r\n");
        }
        eprint!("\r\nvmmc-vmx: Easy Install is done: the install discs are out\r\n");
        Some(vm.clone())
    }
}

struct Tap {
    console: Box<dyn Write + Send>,
    watcher: Sender<Vec<u8>>,
}

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Never blocks: the watcher types from its own thread, since this runs while
        // the UART is locked.
        let _ = self.watcher.send(buf.to_vec());
        self.console.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.console.flush()
    }
}

fn watch(state: &Mutex<State>, output: &Receiver<Vec<u8>>, targets: &Targets) {
    let mut plain = PlainText::default();
    let mut window = String::new();
    while let Ok(chunk) = output.recv() {
        window.push_str(&String::from_utf8_lossy(&plain.filter(&chunk)));
        if window.len() > WINDOW {
            let cut = window.len() - WINDOW / 2;
            let cut = (cut..window.len())
                .find(|&i| window.is_char_boundary(i))
                .unwrap_or(window.len());
            window.drain(..cut);
        }
        let step = {
            let state = state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(install) = &state.vm.config.install else {
                return;
            };
            match install.steps.get(state.next) {
                Some(step) if window.contains(&step.expect) => step.clone(),
                Some(_) => continue,
                None => return,
            }
        };
        // Output from before this point mustn't match the next step.
        window.clear();
        type_keys(targets, step.send.as_bytes());
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state.next += 1;
        eprint!("\r\nvmmc-vmx: Easy Install: step {} done\r\n", state.next);
        if state.done() {
            // Later boots mustn't type these again.
            let vm = &mut state.vm;
            if let Some(install) = &mut vm.config.install {
                install.steps.clear();
                if install.eject_on.is_none() {
                    vm.config.install = None;
                }
            }
            if let Err(e) = vm.save() {
                eprint!("\r\nvmmc-vmx: Easy Install: can't update the VM's file: {e}\r\n");
            }
            return;
        }
    }
}

/// Types `bytes` on the serial console, as fast as the guest takes them.
fn type_keys(targets: &Targets, bytes: &[u8]) {
    let mut pending = bytes.to_vec();
    for _ in 0..2000 {
        if pending.is_empty() {
            return;
        }
        if let Some(target) = targets.get() {
            let sent = target.serial.send(&pending);
            pending.drain(..sent);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Terminal output as plain text: cursor moves become line breaks and every other
/// escape sequence is dropped. Sequences may be split across calls.
#[derive(Default)]
struct PlainText {
    /// 0: text; 1: just saw ESC; 2: in a CSI sequence.
    state: u8,
}

impl PlainText {
    fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match (self.state, byte) {
                (0, 0x1b) => self.state = 1,
                (0, byte) => out.push(byte),
                (1, b'[') => self.state = 2,
                // Two-byte sequences (ESC 7, ESC c, ...).
                (1, _) => self.state = 0,
                (_, b'H' | b'f') => {
                    out.push(b'\n');
                    self.state = 0;
                }
                (_, 0x40..=0x7e) => self.state = 0,
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_become_plain_text() {
        let mut plain = PlainText::default();
        let mut text = plain.filter(b"\x1b[2J\x1b[01;01HTry or \x1b[1");
        text.extend(plain.filter(b"mInstall Ubuntu\x1b[05;10Hgrub>"));
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "\nTry or Install Ubuntu\ngrub>"
        );
    }
}
