//! Several WHP partitions in one process.
//!
//! Windows lets only one partition per process have guest memory mapped at a time; a
//! second one's `WHvMapGpaRange` fails until the first unmaps or is deleted. vmmc runs
//! one VM per worker process, so this only matters for tests. Skipped without a running
//! Windows hypervisor unless `VMMC_REQUIRE_WHP` is set.
#![cfg(windows)]

use std::sync::Mutex;

use hv::PartitionConfig;
use vm_core::Vm;

/// The tests in this file each need the process's one memory-mapped partition.
static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());

fn whp() -> Option<hv_whp::Whp> {
    match hv_whp::Whp::new() {
        Ok(whp) => Some(whp),
        Err(hv::Error::Unavailable(reason)) if std::env::var_os("VMMC_REQUIRE_WHP").is_none() => {
            eprintln!("skipping: {reason}");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

fn vm(whp: &hv_whp::Whp) -> vm_core::Result<Vm<hv_whp::WhpPartition>> {
    Vm::new(whp, &PartitionConfig::default(), &[(0, 0x20_0000)])
}

#[test]
fn vms_can_be_created_one_after_another() {
    let Some(whp) = whp() else { return };
    let _serial = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..3 {
        drop(vm(&whp).expect("each VM after the previous one is gone"));
    }
}

#[test]
fn a_second_vm_in_the_process_works_or_fails_with_a_clear_error() {
    let Some(whp) = whp() else { return };
    let _serial = ONE_VM_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let _first = vm(&whp).expect("first VM");
    // If a future Windows lifts the limit, this just succeeds.
    if let Err(e) = vm(&whp) {
        let message = e.to_string();
        assert!(
            message.contains("one per process"),
            "unclear error: {message}"
        );
    }
}
