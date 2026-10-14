// The private escape between vmmc's display driver and its helper (vmmcdisplay.exe),
// which runs in the user's session: the helper asks what size the host's window wants
// the screen to be, and switches the desktop to it.

#pragma once

#define VMMCDISP_ESCAPE_MAGIC 0x56444d56ul // "VMDV"

// The size the host asks for: Width, Height and Count come back.
#define VMMCDISP_GET_REQUEST 1ul
// Has Windows read the modes again (the monitor reconnects), so that a new size is
// among them.
#define VMMCDISP_REFRESH_MODES 2ul

typedef struct VmmcDispEscape {
    unsigned long Magic;
    unsigned long Command;
    // The size the host asks for; 0 by 0 if it can't (not vmmc's adapter).
    unsigned long Width;
    unsigned long Height;
    // How many times the host has asked: a new count is a new request.
    unsigned long Count;
} VmmcDispEscape;
