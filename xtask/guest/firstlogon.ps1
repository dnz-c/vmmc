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

    # The screen Windows draws on: the firmware's mode, which the display's EDID chose.
    Add-Type -AssemblyName System.Windows.Forms
    $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    Mark "screen=$($screen.Width)x$($screen.Height)"

    # The PS/2 keyboard: xtask types a line into a focused text box once it sees
    # keyboard-ready, and the box reports what arrived.
    $form = New-Object System.Windows.Forms.Form
    $form.Text = 'vmmc keyboard test'
    $form.TopMost = $true
    $box = New-Object System.Windows.Forms.TextBox
    $box.Dock = 'Fill'
    $script:entered = $false
    $box.Add_KeyDown({ if ($_.KeyCode -eq 'Return') { $script:entered = $true } })
    $form.Controls.Add($box)
    $form.Show()
    $form.Activate()
    [void]$box.Focus()
    [System.Windows.Forms.Application]::DoEvents()
    Mark 'keyboard-ready'
    $deadline = (Get-Date).AddSeconds(60)
    while (-not $script:entered -and (Get-Date) -lt $deadline) {
        [System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 50
    }
    Mark "keyboard=$($box.Text)"
    $form.Close()

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
    # Every later boot reports its timings to COM1 (vmmc-boot-report.ps1), for
    # `cargo xtask win-perf`.
    New-Item -ItemType Directory -Force C:\vmmc | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'vmmc-boot-report.ps1') C:\vmmc\boot-report.ps1
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\vmmc\boot-report.ps1'
    $trigger = New-ScheduledTaskTrigger -AtStartup
    Register-ScheduledTask -TaskName 'vmmc boot report' -Action $action -Trigger $trigger `
        -User 'SYSTEM' -RunLevel Highest | Out-Null
    Mark "boot-report=$([bool](Get-ScheduledTask -TaskName 'vmmc boot report' -ErrorAction SilentlyContinue))"
    Mark 'done'
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}

shutdown /s /t 0
