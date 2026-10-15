# vmmc Tools for Windows: installs vmmc's display driver (resolutions, the desktop
# following vmmc's window, a hardware cursor) from the vmmc Tools disc.
#
# The driver is test-signed for now. Windows loads test-signed drivers only with test
# signing on, which Secure Boot doesn't allow: with Secure Boot on, this says so and
# changes nothing. Otherwise it turns test signing on, trusts the disc's test
# certificate, installs the driver, and offers to restart (which test signing needs).

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms

function Say([string] $text, [string] $icon = 'Information') {
    [System.Windows.Forms.MessageBox]::Show($text, 'vmmc Tools', 'OK', $icon) | Out-Null
}

# Administrator rights: start again elevated (Windows asks).
$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
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
        exit 1
    }

    $options = (Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control).SystemStartOptions
    $restart = -not ($options -match 'TESTSIGNING')
    if ($restart) {
        bcdedit /set '{current}' testsigning on | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "bcdedit couldn't turn test signing on" }
    }

    $certificate = Join-Path $disc 'vmmc-test-signing.cer'
    foreach ($store in 'Root', 'TrustedPublisher') {
        Import-Certificate -FilePath $certificate -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
    }

    $output = pnputil /add-driver (Join-Path $disc 'vmmcdisp.inf') /install 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) {
        throw "pnputil failed ($LASTEXITCODE): $output"
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
