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
    Mark "hypervisor=$($system.HypervisorPresent)"

    try { $secureBoot = Confirm-SecureBootUEFI } catch { $secureBoot = 'unsupported' }
    Mark "secureboot=$secureBoot"

    try {
        $tpm = Get-Tpm
        Mark "tpm=$($tpm.TpmPresent) ready=$($tpm.TpmReady)"
    } catch {
        # Where it failed, and what Windows' own tool makes of the TPM.
        Mark "tpm=error ($($_.Exception.Message))"
        Mark "tpm-stack=$($_.Exception.ToString() -replace '\s+', ' ')"
        try {
            $info = tpmtool getdeviceinformation 2>&1 | Out-String
            Mark "tpmtool=$($info -replace '\s+', ' ')"
        } catch {
            Mark "tpmtool=error ($($_.Exception.Message))"
        }
    }

    # The xHCI's tablet, working, and the mice Windows made of it and the PS/2 one.
    $present = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue
    $tablet = $present | Where-Object { $_.InstanceId -like 'USB\VID_1209&PID_0001*' -and $_.Status -eq 'OK' }
    $mice = $present | Where-Object { $_.Class -eq 'Mouse' -and $_.Status -eq 'OK' }
    Mark "usb-tablet=$([bool]$tablet) mice=$(@($mice).Count)"

    $disk = Get-Disk -Number 0
    Mark "disk=$($disk.BusType) $([math]::Round($disk.Size / 1GB))GB"

    # The NAT: fetch xtask's page on the host through the gateway. Right after setup
    # DHCP may still be going, so keep trying for a minute.
    $net = 'failed'
    for ($try = 0; $try -lt 12; $try++) {
        try {
            $page = Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 'http://10.0.2.2:@@HTTP_PORT@@/'
            # RawContent is always text; Content is bytes unless the type says text.
            if ($page.RawContent.EndsWith('@@HTTP_BODY@@')) { $net = 'ok'; break }
            $net = 'wrong-page'
        } catch {
            $net = "failed ($($_.Exception.Message))"
        }
        Start-Sleep -Seconds 5
    }
    $address = (Get-NetIPAddress -AddressFamily IPv4 -PrefixOrigin Dhcp -ErrorAction SilentlyContinue).IPAddress -join ','
    Mark "net=$net address=$address"
    # The internet, if the host has it. Informational only.
    $online = Test-NetConnection -ComputerName www.microsoft.com -Port 443 -InformationLevel Quiet -WarningAction SilentlyContinue
    Mark "internet=$online"

    # Devices without a working driver.
    $broken = Get-PnpDevice -PresentOnly | Where-Object { $_.Status -ne 'OK' }
    foreach ($device in $broken) {
        $status = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName DEVPKEY_Device_ProblemStatus -ErrorAction SilentlyContinue).Data
        Mark "device-problem=$($device.FriendlyName) [$($device.InstanceId)] $($device.Status) problem=$($device.Problem) status=$status"
    }
    Mark 'done'
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}

shutdown /s /t 0
