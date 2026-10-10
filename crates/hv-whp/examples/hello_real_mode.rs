//! Boots a tiny real-mode guest on WHP and prints what it writes to the debug console.
//!
//! ```text
//! cargo run -p hv-whp --example hello_real_mode
//! ```

#[cfg(windows)]
#[path = "../tests/support/hello_guest.rs"]
mod hello_guest;
#[cfg(windows)]
#[path = "../tests/support/real_mode.rs"]
mod real_mode;

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    let whp = match hv_whp::Whp::new() {
        Ok(whp) => whp,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match hello_guest::run(&whp) {
        Ok((exit, output)) => {
            print!("{output}");
            eprintln!("guest exit: {exit:?}");
            if exit == vm_core::VmExit::Stopped(vm_core::StopReason::Requested(0)) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("error: the WHP backend only runs on Windows");
    ExitCode::FAILURE
}
