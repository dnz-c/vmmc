// vmmc display helper (vmmcdisplay.exe): keeps the desktop the size of the host's
// window. It asks vmmc's display driver what size the host wants (vmmcdisp_escape.h)
// and switches the desktop to it. Only a program in the user's session can change the
// desktop's resolution; the driver can only offer it.
//
// It runs twice over. The driver's INF installs it as a service, "vmmcdisplay", which
// starts it in each session as the user who logs on (with "-session"); that copy does
// the work. Run by hand, it does the work in the session it's run in.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <d3dkmthk.h>
#include <userenv.h>
#include <wchar.h>
#include <wtsapi32.h>

#include "vmmcdisp_escape.h"

namespace {

constexpr DWORD POLL_MS = 500;
// After Windows is asked to read the modes again, how long to keep trying a size.
constexpr int RETRIES = 10;
wchar_t SERVICE_NAME[] = L"vmmcdisplay";

// ---- In the user's session ----

// The GDI name ("\\.\DISPLAY1") of the desktop's display on vmmc's adapter.
bool FindDisplay(wchar_t (&name)[32]) {
    DISPLAY_DEVICEW device = {};
    device.cb = sizeof(device);
    for (DWORD i = 0; EnumDisplayDevicesW(nullptr, i, &device, 0); i++) {
        if ((device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) != 0 &&
            wcsstr(device.DeviceID, L"VEN_1234&DEV_1111") != nullptr) {
            wcscpy_s(name, device.DeviceName);
            return true;
        }
        device.cb = sizeof(device);
    }
    return false;
}

// Sends the driver `data`, which it fills in.
bool Ask(const wchar_t* display, VmmcDispEscape* data) {
    D3DKMT_OPENADAPTERFROMGDIDISPLAYNAME open = {};
    wcscpy_s(open.DeviceName, display);
    if (!NT_SUCCESS(D3DKMTOpenAdapterFromGdiDisplayName(&open))) {
        return false;
    }
    D3DKMT_ESCAPE escape = {};
    escape.hAdapter = open.hAdapter;
    escape.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
    escape.pPrivateDriverData = data;
    escape.PrivateDriverDataSize = sizeof(*data);
    NTSTATUS status = D3DKMTEscape(&escape);
    D3DKMT_CLOSEADAPTER close = {};
    close.hAdapter = open.hAdapter;
    D3DKMTCloseAdapter(&close);
    return NT_SUCCESS(status);
}

LONG SetSize(const wchar_t* display, DWORD width, DWORD height) {
    DEVMODEW mode = {};
    mode.dmSize = sizeof(mode);
    if (!EnumDisplaySettingsW(display, ENUM_CURRENT_SETTINGS, &mode)) {
        return DISP_CHANGE_FAILED;
    }
    if (mode.dmPelsWidth == width && mode.dmPelsHeight == height) {
        return DISP_CHANGE_SUCCESSFUL;
    }
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    return ChangeDisplaySettingsExW(display, &mode, nullptr, CDS_UPDATEREGISTRY, nullptr);
}

int FollowHost() {
    // One per session.
    HANDLE single = CreateMutexW(nullptr, TRUE, L"Local\\vmmc display helper");
    if (single == nullptr || GetLastError() == ERROR_ALREADY_EXISTS) {
        return 0;
    }
    // The host's request count when the helper last acted on one: the count at boot
    // is 0, so a desktop only changes once the host asks.
    ULONG handled = 0;
    for (;; Sleep(POLL_MS)) {
        wchar_t display[32];
        if (!FindDisplay(display)) {
            continue;
        }
        VmmcDispEscape request = {VMMCDISP_ESCAPE_MAGIC, VMMCDISP_GET_REQUEST};
        if (!Ask(display, &request) || request.Width == 0 || request.Count == handled) {
            continue;
        }
        handled = request.Count;
        LONG result = SetSize(display, request.Width, request.Height);
        if (result == DISP_CHANGE_BADMODE) {
            // A size Windows hasn't seen: have it read the modes again, then retry.
            VmmcDispEscape refresh = {VMMCDISP_ESCAPE_MAGIC, VMMCDISP_REFRESH_MODES};
            Ask(display, &refresh);
            for (int i = 0; i < RETRIES && result != DISP_CHANGE_SUCCESSFUL; i++) {
                Sleep(POLL_MS);
                if (FindDisplay(display)) {
                    result = SetSize(display, request.Width, request.Height);
                }
            }
        }
    }
}

// ---- The service ----

SERVICE_STATUS_HANDLE g_status_handle = nullptr;
SERVICE_STATUS g_status = {};
HANDLE g_stop = nullptr;

void Report(DWORD state) {
    g_status.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    g_status.dwCurrentState = state;
    g_status.dwControlsAccepted =
        state == SERVICE_RUNNING ? SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN |
                                       SERVICE_ACCEPT_SESSIONCHANGE
                                 : 0;
    SetServiceStatus(g_status_handle, &g_status);
}

// Starts the helper in `session`, as the user logged on there; a session without one
// (the logon screen's) is skipped. A second copy in a session exits at once.
void StartIn(DWORD session) {
    HANDLE token = nullptr;
    if (!WTSQueryUserToken(session, &token)) {
        return;
    }
    wchar_t path[MAX_PATH];
    DWORD length = GetModuleFileNameW(nullptr, path, MAX_PATH);
    void* environment = nullptr;
    if (length != 0 && length < MAX_PATH &&
        CreateEnvironmentBlock(&environment, token, FALSE)) {
        wchar_t command[MAX_PATH + 16];
        swprintf_s(command, L"\"%s\" -session", path);
        STARTUPINFOW startup = {};
        startup.cb = sizeof(startup);
        startup.lpDesktop = const_cast<wchar_t*>(L"winsta0\\default");
        PROCESS_INFORMATION process = {};
        if (CreateProcessAsUserW(token, path, command, nullptr, nullptr, FALSE,
                                 CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW, environment,
                                 nullptr, &startup, &process)) {
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        DestroyEnvironmentBlock(environment);
    }
    CloseHandle(token);
}

DWORD WINAPI Control(DWORD control, DWORD event, void* data, void*) {
    switch (control) {
    case SERVICE_CONTROL_STOP:
    case SERVICE_CONTROL_SHUTDOWN:
        Report(SERVICE_STOP_PENDING);
        SetEvent(g_stop);
        return NO_ERROR;
    case SERVICE_CONTROL_SESSIONCHANGE:
        if (event == WTS_SESSION_LOGON) {
            StartIn(static_cast<WTSSESSION_NOTIFICATION*>(data)->dwSessionId);
        }
        return NO_ERROR;
    case SERVICE_CONTROL_INTERROGATE:
        return NO_ERROR;
    default:
        return ERROR_CALL_NOT_IMPLEMENTED;
    }
}

void WINAPI ServiceMain(DWORD, wchar_t**) {
    g_status_handle = RegisterServiceCtrlHandlerExW(SERVICE_NAME, Control, nullptr);
    if (g_status_handle == nullptr) {
        return;
    }
    g_stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    Report(SERVICE_RUNNING);
    // Users logged on before the service started; later logons come to Control.
    WTS_SESSION_INFOW* sessions = nullptr;
    DWORD count = 0;
    if (WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &sessions, &count)) {
        for (DWORD i = 0; i < count; i++) {
            if (sessions[i].State == WTSActive || sessions[i].State == WTSDisconnected) {
                StartIn(sessions[i].SessionId);
            }
        }
        WTSFreeMemory(sessions);
    }
    WaitForSingleObject(g_stop, INFINITE);
    Report(SERVICE_STOPPED);
}

} // namespace

int WINAPI wWinMain(HINSTANCE, HINSTANCE, PWSTR arguments, int) {
    if (wcsstr(arguments, L"-session") == nullptr) {
        SERVICE_TABLE_ENTRYW services[] = {{SERVICE_NAME, ServiceMain}, {nullptr, nullptr}};
        if (StartServiceCtrlDispatcherW(services)) {
            return 0;
        }
        // Not started as a service: run by hand.
    }
    return FollowHost();
}
