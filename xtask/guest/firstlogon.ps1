# Runs once at the first logon of the unattended Windows install. Writes VMMC-WIN
# markers to COM1 for `cargo xtask win11-test`, then shuts Windows down.

$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Open()
function Mark([string] $text) {
    $port.WriteLine("VMMC-WIN: $text")
}

try {
    Mark "booted $([Environment]::OSVersion.Version)"
    $system = Get-CimInstance Win32_ComputerSystem
    Mark "cpus=$($system.NumberOfLogicalProcessors) memory=$([math]::Round($system.TotalPhysicalMemory / 1GB))GB"
    Mark "model=$($system.Model)"

    try { $secureBoot = Confirm-SecureBootUEFI } catch { $secureBoot = 'unsupported' }
    Mark "secureboot=$secureBoot"

    try {
        $tpm = Get-Tpm
        Mark "tpm=$($tpm.TpmPresent) ready=$($tpm.TpmReady)"
    } catch {
        Mark 'tpm=error'
    }

    $disk = Get-Disk -Number 0
    Mark "disk=$($disk.BusType) $([math]::Round($disk.Size / 1GB))GB"

    $adapters = Get-NetAdapter -Physical -ErrorAction SilentlyContinue | Where-Object Status -eq 'Up'
    if ($adapters) {
        $online = Test-NetConnection -ComputerName www.microsoft.com -Port 443 -InformationLevel Quiet
        Mark "net=$online"
    } else {
        Mark 'net=no-adapter'
    }

    # Devices without a working driver.
    $broken = Get-PnpDevice -PresentOnly | Where-Object { $_.Status -ne 'OK' }
    foreach ($device in $broken) {
        Mark "device-problem=$($device.FriendlyName) [$($device.InstanceId)] $($device.Status)"
    }
    Mark 'done'
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}

shutdown /s /t 0
