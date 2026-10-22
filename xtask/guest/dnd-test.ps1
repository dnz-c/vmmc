# `cargo xtask win-dnd-test`, at each logon, from its test disc: runs the disc's agent
# rather than the installed one, then shows a window over the whole screen. Its left half
# takes drops, and its right half starts a drag of test files when pressed; what happens
# goes to COM1, with the files' SHA-256 hashes for the host to check.

$port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
$port.Encoding = [Text.Encoding]::UTF8
$port.Open()
function Mark([string] $text) {
    $port.WriteLine("VMMC-DND: $text")
}
function Hash([string] $path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower()
}
# Each file under `$path` (or `$path` itself), as "hash relative/path" from `$root`.
function Report([string] $kind, [string] $path, [string] $root) {
    $files = if (Test-Path -LiteralPath $path -PathType Container) {
        Get-ChildItem -LiteralPath $path -Recurse -File | ForEach-Object { $_.FullName }
    } else {
        @($path)
    }
    foreach ($file in $files) {
        $relative = $file.Substring($root.Length).TrimStart('\').Replace('\', '/')
        Mark "$kind $(Hash $file) $relative"
    }
}

try {
    Mark 'logged-on'
    Get-Process vmmcagent -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    $agent = Join-Path $env:TEMP 'vmmcagent.exe'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'vmmcagent.exe') $agent -Force
    Start-Process $agent
    Start-Sleep -Milliseconds 500
    Mark "agent=$([bool](Get-Process vmmcagent -ErrorAction SilentlyContinue))"

    # What the guest drags out: a file, and a folder with a folder in it. (Non-ASCII
    # names are made here: this file is read in the system's code page.)
    $out = Join-Path $env:TEMP 'vmmc-from-guest'
    Remove-Item -Recurse -Force -LiteralPath $out -ErrorAction SilentlyContinue
    $deep = Join-Path $out 'Ordner\tief'
    New-Item -ItemType Directory -Force $deep | Out-Null
    $bytes = New-Object byte[] (3MB + 17)
    (New-Object System.Random 42).NextBytes($bytes)
    [IO.File]::WriteAllBytes((Join-Path $out 'guest file.bin'), $bytes)
    [IO.File]::WriteAllText((Join-Path $deep "$([char]0xe4)rger.txt"), 'aus dem Gast')
    New-Item -ItemType Directory -Force (Join-Path $out 'Ordner\leer') | Out-Null
    Report 'guest-file' $out $out

    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    $screen = [Windows.Forms.Screen]::PrimaryScreen.Bounds
    $form = New-Object Windows.Forms.Form
    $form.FormBorderStyle = 'None'
    $form.StartPosition = 'Manual'
    $form.Bounds = $screen
    $form.TopMost = $true
    $form.AllowDrop = $true
    $form.BackColor = 'White'
    $source = New-Object Windows.Forms.Label
    $source.Text = 'drag from here'
    $source.BackColor = 'LightBlue'
    $source.SetBounds([int]($screen.Width / 2), 0, [int]($screen.Width / 2), $screen.Height)
    $form.Controls.Add($source)
    $form.Add_DragEnter({
        if ($_.Data.GetDataPresent([Windows.Forms.DataFormats]::FileDrop)) {
            $_.Effect = 'Copy'
        }
    })
    $form.Add_DragDrop({
        foreach ($path in $_.Data.GetData([Windows.Forms.DataFormats]::FileDrop)) {
            Report 'dropped-file' $path (Split-Path -Parent $path)
            if (Test-Path -LiteralPath $path -PathType Container) {
                Get-ChildItem -LiteralPath $path -Recurse -Directory | ForEach-Object {
                    $relative = $_.FullName.Substring((Split-Path -Parent $path).Length + 1).Replace('\', '/')
                    Mark "dropped-dir $relative"
                }
            }
        }
        Mark 'dropped'
    })
    $source.Add_MouseDown({
        $files = New-Object System.Collections.Specialized.StringCollection
        [void]$files.Add((Join-Path $out 'guest file.bin'))
        [void]$files.Add((Join-Path $out 'Ordner'))
        $data = New-Object Windows.Forms.DataObject
        $data.SetFileDropList($files)
        Mark 'drag-started'
        $effect = $source.DoDragDrop($data, [Windows.Forms.DragDropEffects]::Copy)
        Mark "drag-done effect=$effect"
    })
    $form.Add_Shown({
        $form.Activate()
        $y = [int]($screen.Height / 2)
        Mark "ready screen=$($screen.Width)x$($screen.Height) target=$([int]($screen.Width / 4)),$y source=$([int]($screen.Width * 3 / 4)),$y"
    })
    [Windows.Forms.Application]::Run($form)
} catch {
    Mark "error=$($_.Exception.Message)"
} finally {
    $port.Close()
}
