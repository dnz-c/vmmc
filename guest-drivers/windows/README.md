# vmmc's Windows guest drivers

| Driver | What it does |
|---|---|
| `display` (`vmmcdisp.sys`) | A display-only driver (WDDM, KMDOD) for vmmc's display adapter. Windows can change resolution: it offers standard resolutions up to 3840x2160 (whatever fits the framebuffer), the monitor's preferred one, and the size of vmmc's window, and sets the one chosen through the adapter's mode registers. vmmc's window follows. The cursor is a hardware one: its image goes to the adapter, and vmmc's window draws it at the host's pointer. |
| `display` (`vmmcdisplay.exe`) | The display driver's helper, in each user's session: the INF installs it as a service ("vmmc Display Helper"), which starts it as each user logs on. When vmmc's window is resized, it switches the desktop to the window's size: the driver tells it the size through a private escape (`vmmcdisp_escape.h`), and only a program in the session can change the desktop's resolution. For a size Windows hasn't offered yet, the driver reconnects the monitor first, so the screen flickers once. |

Without it, Windows draws through the firmware's framebuffer and keeps the resolution
the VM started with (`vmmc-vmx run --resolution`).

## Building

```powershell
cargo xtask guest-drivers
```

It needs MSVC (Visual Studio or its Build Tools, C++ workload) and the Windows Driver
Kit. xtask calls the compiler, linker, `stampinf`, `InfVerif`, `Inf2Cat` and `signtool`
itself, so the WDK's Visual Studio integration doesn't matter. `InfVerif /w` checks the
INF against the rules for drivers Microsoft signs. The package lands in
`target\guest-drivers\windows\vmmcdisp` (`.inf`, `.sys`, `vmmcdisplay.exe`, `.cat`).
`cargo xtask guest-drivers --trace` builds a driver that traces every call from
Windows to vmmc's debug console (`vmmc-vmx run --firmware-log FILE`).

## Signing

xtask signs the driver and its catalog with a self-signed test certificate, "vmmc test
driver signing", which it creates once in your user certificate store and exports to
`target\guest-drivers\vmmc-test-signing.cer`.

Windows loads a test-signed driver only with test signing on, and Secure Boot doesn't
allow test signing. So, until the driver is signed by Microsoft, it's for VMs with
Secure Boot off. For drivers to load in ordinary VMs (Secure Boot on), they need
Microsoft's attestation signature, through the Windows Hardware Dev Center, which
takes an EV code-signing certificate in the company's name.

## Trying it in a VM

`cargo xtask win-display-test` does all of this unattended. By hand:

1. Create the VM with Secure Boot off: `vmmc-vmx run ... --vars-template
   target\native\OVMF_VARS.fd` on its first run (the variable store is copied once).
2. In Windows, as administrator: `bcdedit /set testsigning on`, then restart.
3. Import `vmmc-test-signing.cer` into the machine's Trusted Root and Trusted
   Publishers stores, and run `pnputil /add-driver vmmcdisp.inf /install` in the
   package folder.
4. Settings > Display now lists the resolutions, and the desktop follows vmmc's
   window: the install starts the helper's service, which starts the helper in your
   session.

## Licence

Written for vmmc from Microsoft's documentation of the display-only driver interface,
under vmmc's licence. No sample code was copied.
