# First logon of `cargo xtask win-display-test`: installs the vmmc display driver
# (Setup's specialize pass trusted its test certificate), reports to COM1 what Windows
# makes of it, switches the resolution (the host checks the screen), then shuts down.

$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Open()
function Mark([string] $text) {
    $port.WriteLine("VMMC-DISP: $text")
}

try {
    Mark 'booted'
    $options = (Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control).SystemStartOptions
    Mark "testsigning=$($options -match 'TESTSIGNING')"
    $output = pnputil /add-driver (Join-Path $PSScriptRoot 'vmmcdisp.inf') /install 2>&1 | Out-String
    Mark "pnputil=$LASTEXITCODE $($output -replace '\s+', ' ')"

    # The adapter restarts with the driver.
    $deadline = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Seconds 2
        $adapter = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'PCI\VEN_1234&DEV_1111*' }
        $service = (Get-PnpDeviceProperty -InstanceId $adapter.InstanceId -KeyName DEVPKEY_Device_Service -ErrorAction SilentlyContinue).Data
    } while (($service -ne 'vmmcdisp' -or $adapter.Status -ne 'OK') -and (Get-Date) -lt $deadline)
    Mark "driver=$service status=$($adapter.Status) problem=$($adapter.Problem)"
    if ($service -ne 'vmmcdisp') {
        # Why not: the certificate, and the log of device installs.
        foreach ($store in 'Root', 'TrustedPublisher') {
            $trusted = Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object { $_.Subject -eq 'CN=vmmc test driver signing' }
            Mark "certificate-in-$store=$([bool]$trusted)"
        }
        Select-String -Path 'C:\Windows\INF\setupapi.dev.log' -Pattern 'vmmcdisp|!!!' |
            Select-Object -Last 25 | ForEach-Object { Mark "setupapi: $($_.Line.Trim())" }
    }

    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class VmmcDisplay {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
        public int dmFields;
        public int dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
        public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels;
        public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
        public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2;
        public int dmPanningWidth, dmPanningHeight;
    }

    const int ENUM_CURRENT_SETTINGS = -1;
    const int DM_PELSWIDTH = 0x80000;
    const int DM_PELSHEIGHT = 0x100000;

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern bool EnumDisplaySettings(string device, int mode, ref DEVMODE devMode);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern int ChangeDisplaySettingsEx(string device, ref DEVMODE devMode, IntPtr window, int flags, IntPtr parameter);

    static DEVMODE Empty() {
        var mode = new DEVMODE();
        mode.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
        return mode;
    }

    // The resolutions Windows offers, as WxH, smallest first.
    public static string Modes() {
        var seen = new List<string>();
        var mode = Empty();
        for (int i = 0; EnumDisplaySettings(null, i, ref mode); i++) {
            string size = mode.dmPelsWidth + "x" + mode.dmPelsHeight;
            if (!seen.Contains(size)) seen.Add(size);
        }
        return string.Join(",", seen);
    }

    public static string Current() {
        var mode = Empty();
        EnumDisplaySettings(null, ENUM_CURRENT_SETTINGS, ref mode);
        return mode.dmPelsWidth + "x" + mode.dmPelsHeight;
    }

    // DISP_CHANGE_SUCCESSFUL is 0.
    public static int Set(int width, int height) {
        var mode = Empty();
        EnumDisplaySettings(null, ENUM_CURRENT_SETTINGS, ref mode);
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = height;
        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        return ChangeDisplaySettingsEx(null, ref mode, IntPtr.Zero, 0, IntPtr.Zero);
    }
}
'@
    Mark "modes=$([VmmcDisplay]::Modes())"
    Mark "before=$([VmmcDisplay]::Current())"
    $result = [VmmcDisplay]::Set(1600, 900)
    Start-Sleep -Seconds 3
    Mark "set=$result now=$([VmmcDisplay]::Current())"
    # The host takes a screenshot now.
    Mark 'mode-set'
    # The host checks the screen, then asks for its window's size (1500x844, a size
    # Windows doesn't know yet): the helper, which the driver's service started in this
    # session, switches the desktop to it.
    Start-Sleep -Seconds 5
    $session = (Get-Process -Id $PID).SessionId
    $service = Get-Service vmmcdisplay -ErrorAction SilentlyContinue
    $helper = Get-Process vmmcdisplay -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session }
    Mark "service=$($service.Status) helper=$([bool]$helper)"
    Mark 'helper-started'
    $deadline = (Get-Date).AddSeconds(60)
    while ([VmmcDisplay]::Current() -ne '1500x844' -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    Mark "fitted=$([VmmcDisplay]::Current())"
    Mark 'fit-checked'
    Start-Sleep -Seconds 10
    # The next boot logs on by itself, and the host checks Windows gives the driver its
    # pointer on the desktop.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon AutoAdminLogon '1'
    Set-ItemProperty $winlogon DefaultUserName 'vmmc'
    Set-ItemProperty $winlogon DefaultPassword 'vmmc'
    Remove-ItemProperty $winlogon AutoLogonCount -ErrorAction SilentlyContinue
    # Windows shows the pointer, and so hands it over, only once the mouse moves: at
    # that logon, a script moves it a little, and reports whether the service started
    # the helper.
    New-Item -ItemType Directory -Force C:\vmmc | Out-Null
    Set-Content C:\vmmc\nudge.ps1 @'
Add-Type -Namespace Vmmc -Name Mouse -MemberDefinition '[DllImport("user32.dll")] public static extern void mouse_event(int flags, int dx, int dy, int data, System.IntPtr extra);'
Start-Sleep -Seconds 15
[Vmmc.Mouse]::mouse_event(1, 20, 20, 0, [System.IntPtr]::Zero)
$session = (Get-Process -Id $PID).SessionId
$helper = Get-Process vmmcdisplay -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session }
$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Open()
$port.WriteLine("VMMC-DISP: logon helper=$([bool]$helper)")
$port.Close()
'@
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run' vmmcnudge `
        'powershell -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File C:\vmmc\nudge.ps1'
    Mark 'done'
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}

shutdown /s /t 0
