#!/bin/sh
# vmmc Tools for Linux: a desktop that runs software from inserted media offers to run
# this ("Run Software" in GNOME, where autorun is on; it's off by default since GNOME
# 45). It runs the install script as root, which asks for the user's password first.
here=$(cd "$(dirname "$0")" && pwd)
exec pkexec /bin/sh "$here/install-linux.sh"
