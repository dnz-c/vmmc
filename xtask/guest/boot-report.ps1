# Runs at every boot of the test Windows, as a scheduled task the first-logon script
# registers. Reports to COM1 how long the boot took and a few timings that depend on
# how the hypervisor is set up, for `cargo xtask win-perf`.

$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Open()
$invariant = [Globalization.CultureInfo]::InvariantCulture
function Mark([string] $format) {
    $text = [string]::Format($invariant, $format, $args)
    $port.WriteLine("VMMC-PERF: $text")
}

try {
    $os = Get-CimInstance Win32_OperatingSystem
    Mark 'boot uptime_s={0:F1}' ((Get-Date) - $os.LastBootUpTime).TotalSeconds

    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Threading;

public static class VmmcBench {
    // Nanoseconds per QueryPerformanceCounter: cheap with a TSC the guest can trust,
    // an exit per call with the ACPI PM timer or the HPET.
    public static double CounterNs() {
        const int n = 2000000;
        long sum = 0;
        var watch = Stopwatch.StartNew();
        for (int i = 0; i < n; i++) sum += Stopwatch.GetTimestamp();
        watch.Stop();
        GC.KeepAlive(sum);
        return watch.Elapsed.TotalMilliseconds * 1e6 / n;
    }

    // Milliseconds per Sleep(1): how precisely the guest's timer interrupts arrive.
    public static double SleepMs() {
        const int n = 200;
        var watch = Stopwatch.StartNew();
        for (int i = 0; i < n; i++) Thread.Sleep(1);
        return watch.Elapsed.TotalMilliseconds / n;
    }

    // Microseconds per wake-up between two threads, which mostly run on different
    // vCPUs: interrupts between processors.
    public static double WakeUs() {
        const int n = 20000;
        var ping = new AutoResetEvent(false);
        var pong = new AutoResetEvent(false);
        var other = new Thread(() => { for (int i = 0; i < n; i++) { ping.WaitOne(); pong.Set(); } });
        other.Start();
        var watch = Stopwatch.StartNew();
        for (int i = 0; i < n; i++) { ping.Set(); pong.WaitOne(); }
        watch.Stop();
        other.Join();
        return watch.Elapsed.TotalMilliseconds * 1000 / (2 * n);
    }

    // MB/s of SHA-256: plain computation, which shouldn't depend on the hypervisor.
    public static double HashMBps() {
        var data = new byte[64 << 20];
        new Random(1).NextBytes(data);
        var watch = Stopwatch.StartNew();
        using (var sha = SHA256.Create()) sha.ComputeHash(data);
        return 64 / watch.Elapsed.TotalSeconds;
    }

    // GB/s of touching fresh memory: page faults, and the hypervisor backing new pages.
    public static double TouchGBps() {
        var watch = Stopwatch.StartNew();
        var buffer = new byte[1 << 30];
        for (int i = 0; i < buffer.Length; i += 4096) buffer[i] = 1;
        return 1 / watch.Elapsed.TotalSeconds;
    }
}
'@
    Mark 'counter frequency_hz={0} ns_per_read={1:F1}' ([Diagnostics.Stopwatch]::Frequency) ([VmmcBench]::CounterNs())
    Mark 'sleep ms_per_sleep1={0:F2}' ([VmmcBench]::SleepMs())
    Mark 'wake us_per_wake={0:F1}' ([VmmcBench]::WakeUs())
    Mark 'hash MBps={0:F0}' ([VmmcBench]::HashMBps())
    Mark 'touch GBps={0:F2}' ([VmmcBench]::TouchGBps())
    $hypervisor = (Get-CimInstance Win32_ComputerSystem).HypervisorPresent
    Mark 'hypervisor present={0}' $hypervisor
    Mark 'done'
} catch {
    Mark 'error={0}' $_.Exception.Message
} finally {
    $port.Close()
}
