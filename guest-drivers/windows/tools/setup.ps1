# vmmc Tools for Windows: installs vmmc's display driver (resolutions, the desktop
# following vmmc's window, a hardware cursor) from the vmmc Tools disc.
#
# The driver is test-signed for now. Windows loads test-signed drivers only with test
# signing on, which Secure Boot doesn't allow: with Secure Boot on, this says so and
# changes nothing. Otherwise it turns test signing on, trusts the disc's test
# certificate, installs the driver, and offers to restart (which test signing needs).
#
# -Quiet: no dialogs and no restart; what happens goes to the output, and the exit code
# says how it went (0 installed, 1 failed, 2 Secure Boot is on, 3010 restart needed).

param([switch] $Quiet)

$ErrorActionPreference = 'Stop'
if (-not $Quiet) {
    Add-Type -AssemblyName System.Windows.Forms
}

function Say([string] $text, [string] $icon = 'Information') {
    if ($Quiet) {
        Write-Output "vmmc Tools: $text"
    } else {
        [System.Windows.Forms.MessageBox]::Show($text, 'vmmc Tools', 'OK', $icon) | Out-Null
    }
}

# Runs a program, and stops with `what` if it fails. Returns its exit code.
function Run([string] $what, [string] $program, [string[]] $arguments, [int[]] $ok = @(0)) {
    # What the program writes to stderr isn't an error of this script's (with 'Stop',
    # Windows PowerShell would stop at its first line).
    $ErrorActionPreference = 'Continue'
    $output = (& $program @arguments 2>&1 | ForEach-Object { "$_" }) -join "`n"
    if ($ok -notcontains $LASTEXITCODE) {
        throw "$what failed (exit code $LASTEXITCODE): $($output.Trim())"
    }
    $LASTEXITCODE
}

# Administrator rights: start again elevated (Windows asks).
$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    if ($Quiet) {
        Write-Output 'vmmc Tools: needs administrator rights'
        exit 1
    }
    Start-Process powershell -Verb RunAs -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    exit
}

$disc = Split-Path -Parent $PSCommandPath
try {
    $secureBoot = $false
    try { $secureBoot = Confirm-SecureBootUEFI } catch { }
    if ($secureBoot) {
        Say ("Secure Boot is on in this VM, and vmmc's display driver isn't signed by Microsoft yet, " +
             "so Windows won't load it.`n`nTo use it now: shut this VM down, turn Secure Boot off in " +
             "vmmc's settings for it, start it, and run vmmc Tools again.") 'Warning'
        exit 2
    }

    $options = (Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control).SystemStartOptions
    $restart = -not ($options -match 'TESTSIGNING')
    if ($restart) {
        Run 'Turning test signing on (bcdedit)' bcdedit @('/set', '{current}', 'testsigning', 'on') | Out-Null
    }

    # certutil, not Import-Certificate: Windows denies the latter the machine's stores
    # even to administrators.
    $certificate = Join-Path $disc 'vmmc-test-signing.cer'
    foreach ($store in 'Root', 'TrustedPublisher') {
        Run "Trusting the driver's test certificate ($store)" certutil @('-addstore', '-f', $store, $certificate) | Out-Null
    }

    # 3010: installed, and Windows wants a restart.
    $code = Run 'Installing the display driver (pnputil)' pnputil @('/add-driver', (Join-Path $disc 'vmmcdisp.inf'), '/install') @(0, 3010)
    if ($code -eq 3010) { $restart = $true }

    if ($Quiet) {
        Write-Output "vmmc Tools: installed$(if ($restart) { '; restart needed' })"
        exit $(if ($restart) { 3010 } else { 0 })
    }
    if ($restart) {
        $answer = [System.Windows.Forms.MessageBox]::Show(
            "vmmc Tools are installed. Windows has to restart once before the display driver works.`n`nRestart now?",
            'vmmc Tools', 'YesNo', 'Question')
        if ($answer -eq 'Yes') { Restart-Computer -Force }
    } else {
        Say 'vmmc Tools are installed: Windows now follows the size of vmmc''s window.'
    }
} catch {
    Say "vmmc Tools couldn't be installed:`n`n$($_.Exception.Message)" 'Error'
    exit 1
}
