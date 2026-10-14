//! `cargo xtask win-perf [RUNS]`: boots the Windows that `win11-test` installed, RUNS
//! times (3 by default) without and RUNS times with Hyper-V enlightenments, taking
//! turns, and compares what each boot measured:
//!
//! - **From the host:** seconds from starting the VM to the guest's boot report, the
//!   share of one host CPU the VM uses while Windows sits at its logon screen, and how
//!   long Windows takes to shut down after the power button.
//! - **From the guest** (`guest/boot-report.ps1`, a task at every boot): its uptime at
//!   the report, and timings of counter reads, short sleeps, wake-ups between threads,
//!   hashing and touching new memory.
//!
//! It runs a release build, as users do.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::firmware::Session;
use crate::{build_vmmc_vmx_release, image_dir, native, windows, workspace};

const MARKER: &str = "VMMC-PERF: ";
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
const REPORT_TIMEOUT: Duration = Duration::from_secs(300);
/// Between the report and the idle measurement, for the logon screen to settle.
const SETTLE: Duration = Duration::from_secs(20);
const IDLE: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(180);

/// What one boot measured, by name (`host.boot_s`, `wake.us_per_wake`, ...).
struct Run {
    enlightened: bool,
    values: BTreeMap<String, f64>,
    /// Values that aren't numbers, such as `hypervisor.present=True`.
    notes: Vec<String>,
}

pub fn test(runs: usize) -> Result<(), String> {
    let workspace = workspace();
    let native = native::fetch(&workspace)?;
    let disk = image_dir()?.join("win11-test-disk.vmdk");
    let vm_dir = workspace.join("target").join("win11-test").join("vm");
    if !disk.is_file() || !vm_dir.is_dir() {
        return Err("no installed Windows: run `cargo xtask win11-test` first".into());
    }
    let exe = build_vmmc_vmx_release(&workspace)?;
    let mut results = Vec::new();
    for round in 1..=runs {
        for enlightened in [false, true] {
            eprintln!(
                "xtask: run {round} of {runs}, {}",
                if enlightened {
                    "with enlightenments"
                } else {
                    "without enlightenments"
                }
            );
            let run = boot(&exe, &native, &vm_dir, &disk, enlightened)?;
            for (name, value) in &run.values {
                eprintln!("xtask:   {name} = {value}");
            }
            results.push(run);
        }
    }
    eprintln!("{}", summary(&results));
    Ok(())
}

fn boot(
    exe: &Path,
    native: &Path,
    vm_dir: &Path,
    disk: &Path,
    enlightened: bool,
) -> Result<Run, String> {
    let mut command = Command::new(exe);
    command
        .arg("run")
        .arg("--firmware")
        .arg(native.join("OVMF_CODE.fd"))
        .arg("--vars-template")
        .arg(native.join("OVMF_VARS.secboot.fd"))
        .arg("--tpm")
        .arg("--vm-dir")
        .arg(vm_dir)
        .arg("--disk")
        .arg(format!("{},nvme", disk.display()))
        .args(["--cpus", windows::CPUS, "--memory", windows::MEMORY_MIB])
        .args(["--nic", "e1000e", "--resolution", windows::RESOLUTION]);
    if !enlightened {
        command.arg("--no-enlightenments");
    }
    let started = Instant::now();
    let mut vm = Session::spawn(command)?;
    let result = (|| {
        let first = format!("{MARKER}boot");
        let at = vm
            .expect(&first, 0, BOOT_TIMEOUT)
            .map_err(|e| format!("{e} (was it installed by this version of `win11-test`?)"))?;
        let boot_s = started.elapsed().as_secs_f64();
        let end = vm.expect(&format!("{MARKER}done"), at, REPORT_TIMEOUT)?;
        let report = vm.text(at - first.len(), end);
        if let Some(line) = report.lines().find(|line| line.contains("error=")) {
            return Err(format!("the boot report failed: {}", line.trim()));
        }
        let mut run = parse(&report, enlightened);
        run.values.insert("host.boot_s".into(), boot_s);

        thread::sleep(SETTLE);
        let (cpu, from) = (cpu_ms(vm.pid())?, Instant::now());
        thread::sleep(IDLE);
        let idle = (cpu_ms(vm.pid())? - cpu) / from.elapsed().as_secs_f64() / 10.0;
        run.values.insert("host.idle_cpu_pct".into(), idle);

        // The ACPI power button: Windows shuts down from its logon screen.
        let down = Instant::now();
        vm.send("\x01p")?;
        match vm.wait(SHUTDOWN_TIMEOUT) {
            Some(_) => {
                run.values
                    .insert("host.shutdown_s".into(), down.elapsed().as_secs_f64());
            }
            None => run.notes.push("didn't shut down; killed".into()),
        }
        Ok(run)
    })();
    if result.is_err() {
        vm.kill();
    }
    result
}

/// The report's `VMMC-PERF: name key=value ...` lines, as `name.key` values.
fn parse(report: &str, enlightened: bool) -> Run {
    let mut run = Run {
        enlightened,
        values: BTreeMap::new(),
        notes: Vec::new(),
    };
    for line in report.lines() {
        let Some((_, rest)) = line.split_once(MARKER) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        for word in words {
            let Some((key, value)) = word.split_once('=') else {
                continue;
            };
            match value.parse::<f64>() {
                Ok(number) => {
                    run.values.insert(format!("{name}.{key}"), number);
                }
                Err(_) => run.notes.push(format!("{name}.{key}={value}")),
            }
        }
    }
    run
}

/// Milliseconds of CPU time process `pid` has used.
fn cpu_ms(pid: u32) -> Result<f64, String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("[int64](Get-Process -Id {pid}).TotalProcessorTime.TotalMilliseconds"),
        ])
        .output()
        .map_err(|e| format!("can't run PowerShell: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse()
        .map_err(|_| format!("no CPU time for process {pid}: {text:?}"))
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[middle]),
        _ => Some((values[middle - 1] + values[middle]) / 2.0),
    }
}

/// Each measurement's median without and with enlightenments, and the notes.
fn summary(runs: &[Run]) -> String {
    let names: std::collections::BTreeSet<&String> =
        runs.iter().flat_map(|run| run.values.keys()).collect();
    let mut text = format!(
        "\n{:<28} {:>14} {:>14}\n",
        "median of each", "without", "with"
    );
    for name in names {
        let pick = |enlightened: bool| {
            median(
                runs.iter()
                    .filter(|run| run.enlightened == enlightened)
                    .filter_map(|run| run.values.get(name).copied())
                    .collect(),
            )
            .map_or("-".to_owned(), |value| format!("{value:.2}"))
        };
        text += &format!("{name:<28} {:>14} {:>14}\n", pick(false), pick(true));
    }
    for (enlightened, label) in [(false, "without"), (true, "with")] {
        let mut notes: Vec<&String> = runs
            .iter()
            .filter(|run| run.enlightened == enlightened)
            .flat_map(|run| &run.notes)
            .collect();
        notes.sort();
        notes.dedup();
        if !notes.is_empty() {
            text += &format!(
                "notes {label}: {}\n",
                notes
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lines_become_named_values() {
        let report = "VMMC-PERF: boot uptime_s=41.5\r\nnoise\nVMMC-PERF: counter frequency_hz=10000000 ns_per_read=21.7\nVMMC-PERF: hypervisor present=True\nVMMC-PERF: done\n";
        let run = parse(report, true);
        assert_eq!(run.values["boot.uptime_s"], 41.5);
        assert_eq!(run.values["counter.frequency_hz"], 10_000_000.0);
        assert_eq!(run.values["counter.ns_per_read"], 21.7);
        assert_eq!(run.notes, ["hypervisor.present=True"]);
    }

    #[test]
    fn medians_and_the_summary() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(Vec::new()), None);
        let runs = [
            parse("VMMC-PERF: boot uptime_s=40", false),
            parse("VMMC-PERF: boot uptime_s=30", true),
        ];
        let text = summary(&runs);
        assert!(text.contains("boot.uptime_s"));
        assert!(text.contains("40.00") && text.contains("30.00"), "{text}");
    }
}
