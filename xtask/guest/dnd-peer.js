// `cargo xtask ubuntu-dnd-test`'s peer in the guest: a GTK 4 window over the whole
// screen, a native Wayland program. It copies text to the clipboard and waits for the
// host's; its left half takes drops, and its right half starts a drag of the test files
// (in the folder given) when pressed and moved. What happens goes to COM1, with files'
// SHA-256 hashes for the host to check.

import Gdk from 'gi://Gdk?version=4.0';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk?version=4.0';

const out = Gio.File.new_for_path(ARGV[0]);
const encoder = new TextEncoder();

function mark(text) {
    const stream = Gio.File.new_for_path('/dev/ttyS0').append_to(Gio.FileCreateFlags.NONE, null);
    stream.write_all(encoder.encode(`VMMC-DND: ${text}\n`), null);
    stream.close(null);
}

// Each file under `file` (or `file` itself), as "hash relative/path" from `root`; with
// folders as "dropped-dir path" when they're dropped ones.
function report(kind, file, root) {
    const info = file.query_info('standard::type', Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
    const relative = root.get_relative_path(file);
    if (info.get_file_type() === Gio.FileType.DIRECTORY) {
        if (kind === 'dropped-file')
            mark(`dropped-dir ${relative}`);
        const children = file.enumerate_children('standard::name', Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
        let child;
        while ((child = children.next_file(null)) !== null)
            report(kind, file.get_child(child.get_name()), root);
    } else {
        const [bytes] = file.load_bytes(null);
        mark(`${kind} ${GLib.compute_checksum_for_bytes(GLib.ChecksumType.SHA256, bytes)} ${relative}`);
    }
}

const app = new Gtk.Application({application_id: 'org.vmmc.DragTest'});
app.connect('activate', () => {
    const window = new Gtk.ApplicationWindow({application: app, title: 'vmmc drag test'});
    const box = new Gtk.Box({orientation: Gtk.Orientation.HORIZONTAL, homogeneous: true});
    const target = new Gtk.Label({label: 'drop here', hexpand: true, vexpand: true});
    const source = new Gtk.Label({label: 'drag from here', hexpand: true, vexpand: true});
    box.append(target);
    box.append(source);
    window.set_child(box);

    const drop = Gtk.DropTarget.new(Gdk.FileList.$gtype, Gdk.DragAction.COPY);
    drop.connect('drop', (_target, value) => {
        for (const file of value.get_files())
            report('dropped-file', file, file.get_parent());
        mark('dropped');
        return true;
    });
    target.add_controller(drop);

    const drag = new Gtk.DragSource({actions: Gdk.DragAction.COPY});
    drag.connect('prepare', () => {
        mark('drag-started');
        const uris = [out.get_child('guest file.bin'), out.get_child('Ordner')]
            .map(file => `${file.get_uri()}\r\n`)
            .join('');
        return Gdk.ContentProvider.new_for_bytes('text/uri-list', new GLib.Bytes(encoder.encode(uris)));
    });
    drag.connect('drag-cancel', () => {
        mark('drag-done effect=None');
        return false;
    });
    drag.connect('drag-end', (_source, _drag, deleteData) => {
        if (deleteData)
            mark('drag-done effect=Move');
    });
    source.add_controller(drag);

    window.fullscreen();
    window.present();
    // GNOME doesn't focus a window that a program started at logon shows (no activation
    // token), and Wayland lets only the focused program set the clipboard: the host
    // clicks on it.
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 2000, () => {
        mark('peer-shown');
        return GLib.SOURCE_REMOVE;
    });
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
        if (!window.is_active)
            return GLib.SOURCE_CONTINUE;
        mark('peer-focused');
        startClipboard();
        return GLib.SOURCE_REMOVE;
    });

    // Focused: the clipboard, then the layout for the drags.
    const startClipboard = () => {
        const clipboard = Gdk.Display.get_default().get_clipboard();
        clipboard.connect('changed', () => {
            mark(`clipboard-changed local=${clipboard.is_local()} formats=${clipboard.get_formats().to_string()}`);
        });
        clipboard.set_content(Gdk.ContentProvider.new_for_bytes('text/plain;charset=utf-8',
            new GLib.Bytes(encoder.encode('vmmc clipboard from the guest'))));
        mark('clipboard-copied');
        let tries = 0;
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            clipboard.read_text_async(null, (_clipboard, result) => {
                let text = null;
                try {
                    text = clipboard.read_text_finish(result);
                } catch (e) {
                    text = `(${e.message}; formats ${clipboard.get_formats().to_string()})`;
                }
                tries++;
                if (text === 'vmmc clipboard from the host' || tries === 120) {
                    mark(`clipboard=${text}`);
                    const geometry = Gdk.Display.get_default().get_monitors().get_item(0).get_geometry();
                    const [w, h] = [geometry.width, geometry.height];
                    mark(`ready screen=${w}x${h} target=${Math.floor(w / 4)},${Math.floor(h / 2)} source=${Math.floor(w * 3 / 4)},${Math.floor(h / 2)}`);
                    tries = -1;
                }
            });
            return tries >= 0 && tries < 120 ? GLib.SOURCE_CONTINUE : GLib.SOURCE_REMOVE;
        });
    };
});
app.run([]);
