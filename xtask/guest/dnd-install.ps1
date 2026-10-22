# First logon of the Windows `cargo xtask win-dnd-test` installs once: installs vmmc
# Tools (Secure Boot is on, so that's the agent alone), has later boots log on by
# themselves and run the script on whatever test disc is in, then shuts down.

$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Open()
function Mark([string] $text) {
    $port.WriteLine("VMMC-DND: $text")
}

try {
    Mark 'booted'
    $output = & (Join-Path $PSScriptRoot 'setup.ps1') -Quiet 2>&1 | Out-String
    Mark "setup: $LASTEXITCODE $($output -replace '\s+', ' ')"
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon AutoAdminLogon '1'
    Set-ItemProperty $winlogon DefaultUserName 'vmmc'
    Set-ItemProperty $winlogon DefaultPassword 'vmmc'
    Remove-ItemProperty $winlogon AutoLogonCount -ErrorAction SilentlyContinue
    # At each logon, the test disc's script, whichever letter the disc has.
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run' vmmctest `
        'cmd /c for %d in (D E F G H I J) do @if exist %d:\vmmc-test.ps1 start "" powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File %d:\vmmc-test.ps1'
    Mark 'done'
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}

shutdown /s /t 0
