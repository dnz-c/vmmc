# Checking the app by hand

What the automated tests can't drive: the app's real window, its keyboard grab, full
screen, and Windows' drag and drop from Explorer. Go through this before a release, and
after changing the app. Each line says what to do and what should happen.

## Library and tabs

- [ ] **New VM** with no folder chosen: the VM appears in the library, its tab opens.
- [ ] Move a VM to a library folder (its page, **Library folder**, **Move**): the
      library groups it under the folder's heading.
- [ ] Close the app with two tabs open, start it again: the same tabs open.
- [ ] **Remove from library**: the VM goes from the library; its folder stays on disk.

## New VM and Easy Install

- [ ] Windows 11 (or 10) with **Easy Install**, a user and a password: it installs with
      no questions and reaches the desktop, logged on; the install discs are out of
      the settings afterwards.
- [ ] Ubuntu with **Easy Install**: it installs, powers off, starts from its disk, and
      the user logs on with the password.

## Console

- [ ] Click the screen: the keyboard goes to the guest (the Windows key and Alt+Tab
      too); **Ctrl+Alt** gives it back, as the toolbar's hint says.
- [ ] The pointer moves in and out of the screen freely; clicks land where they show.
- [ ] **Full screen**, then Ctrl+Alt+Enter and Esc: in and out; the bar at the top
      shows on entering and when the pointer touches the top edge.
- [ ] **Fit guest to window** with vmmc's display driver in Windows, and in an Ubuntu
      VM made with the Ubuntu preset (virtio-gpu): resizing the window resizes the
      desktop.
- [ ] **Ctrl+Alt+Del**, **Shut down**, **Power off**, **Reset** do what they say.

## Settings

- [ ] Change processors, memory and the DVD drive's ISO, **Save**, start: the guest
      sees them.
- [ ] Turn Secure Boot off and on: the VM still boots (from its fallback loader).
- [ ] **Add disk…** (NVMe, then SATA): the guest sees a new empty disk.
- [ ] **Expand…** a disk: the guest sees the bigger disk (free space at its end).
- [ ] **Remove** a disk: it's gone from the VM, its file stays in the folder.
- [ ] Network adapter off: the guest has no network; on as virtio-net: Linux has it.

## Snapshots, clones, export

- [ ] **Take snapshot…** while the VM runs: it pauses a moment; the snapshot shows
      "with the VM's memory", marked as the one the VM goes on from.
- [ ] Change something in the guest, **Revert to it**: the guest is back as it was,
      running.
- [ ] **Delete** a snapshot with the VM off: its disk file merges (the folder has one
      `-00000N.vmdk` fewer).
- [ ] **Suspend**: the page says suspended; **Resume** brings the guest back as it was.
- [ ] **Clone…** a snapshot as a linked clone, and the VM (off) as a full clone: both
      start; the linked clone's snapshot can't be deleted.
- [ ] **Export…** to an `.ova`, then **Import VM** that `.ova`: the imported VM starts.

## vmmc Tools

- [ ] **Install vmmc Tools** in Windows: AutoPlay offers setup; afterwards the
      clipboard goes both ways.
- [ ] **Install vmmc Tools** in Ubuntu: the command the app shows installs the agent;
      afterwards the clipboard goes both ways, and still does after logging off and on.
- [ ] Drag a file and a folder from Explorer onto the console (Windows and Ubuntu
      guests): they land where the pointer is.
- [ ] Drag a file out of the guest past the console's edge onto the desktop: it lands
      there.

## Moving over from VMware

- [ ] **Import VM** with a VMware VM's `.vmx` (shut down in VMware): it starts in vmmc;
      its first snapshot is "Imported from VMware"; the VMware folder's files keep
      their times and sizes.
