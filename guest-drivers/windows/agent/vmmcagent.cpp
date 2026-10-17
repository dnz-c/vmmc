// vmmc agent (vmmcagent.exe): shares the clipboard's text between a Windows guest and
// the host. vmmc's worker listens on the guest's second serial port (COM2), and frames
// go both ways: "\x1bVCLIP", the text's length (4 bytes, little-endian), then the text
// in UTF-8 (the ipc crate's clipboard module is the other end).
//
// The vmmc Tools setup installs it in Program Files and starts it at each logon (the
// HKLM Run key). It needs no driver, so it works with Secure Boot on. One copy runs per
// session; the first to open COM2 has it.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <cstdint>
#include <cstring>
#include <cwchar>
#include <memory>
#include <string>
#include <vector>

namespace {

constexpr char MAGIC[6] = {0x1b, 'V', 'C', 'L', 'I', 'P'};
constexpr size_t HEADER = sizeof(MAGIC) + 4;
// The longest text shared (as the worker's).
constexpr size_t MAX_TEXT = 512 * 1024;
// Text from the host, for the window to put on the clipboard: lParam is a
// std::wstring*.
constexpr UINT WM_FROM_HOST = WM_APP + 1;
constexpr DWORD RETRY_MS = 5000;
// The clipboard is read this long after it changes, not at once: the program that set
// it may still have it open (to flush it, say), and a burst of changes is read once.
constexpr UINT_PTR READ_TIMER = 1;
constexpr UINT READ_DELAY_MS = 150;

HANDLE g_port = INVALID_HANDLE_VALUE;
// The clipboard's sequence number after the agent last set it: that change isn't the
// user's, and doesn't go back to the host.
DWORD g_own_sequence = 0;

// What the writer sends next: only the latest text matters.
CRITICAL_SECTION g_outgoing_lock;
std::string g_outgoing;
bool g_has_outgoing = false;
HANDLE g_outgoing_ready = nullptr;

HANDLE OpenPort() {
    HANDLE port = CreateFileW(L"\\\\.\\COM2", GENERIC_READ | GENERIC_WRITE, 0, nullptr,
                              OPEN_EXISTING, FILE_FLAG_OVERLAPPED, nullptr);
    if (port == INVALID_HANDLE_VALUE) {
        return port;
    }
    DCB dcb = {};
    dcb.DCBlength = sizeof(dcb);
    GetCommState(port, &dcb);
    dcb.BaudRate = CBR_115200;
    dcb.ByteSize = 8;
    dcb.Parity = NOPARITY;
    dcb.StopBits = ONESTOPBIT;
    dcb.fBinary = TRUE;
    dcb.fOutxCtsFlow = FALSE;
    dcb.fOutxDsrFlow = FALSE;
    dcb.fOutX = FALSE;
    dcb.fInX = FALSE;
    dcb.fDtrControl = DTR_CONTROL_ENABLE;
    dcb.fRtsControl = RTS_CONTROL_ENABLE;
    SetCommState(port, &dcb);
    SetupComm(port, 64 * 1024, 64 * 1024);
    // A read returns once anything has come, or after a second.
    COMMTIMEOUTS timeouts = {};
    timeouts.ReadIntervalTimeout = MAXDWORD;
    timeouts.ReadTotalTimeoutMultiplier = MAXDWORD;
    timeouts.ReadTotalTimeoutConstant = 1000;
    SetCommTimeouts(port, &timeouts);
    return port;
}

std::wstring FromUtf8(const char* text, size_t len) {
    if (len == 0) {
        return {};
    }
    int wide = MultiByteToWideChar(CP_UTF8, 0, text, static_cast<int>(len), nullptr, 0);
    std::wstring out(static_cast<size_t>(wide), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, text, static_cast<int>(len), out.data(), wide);
    return out;
}

std::string ToUtf8(const std::wstring& text) {
    if (text.empty()) {
        return {};
    }
    int len = WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()),
                                  nullptr, 0, nullptr, nullptr);
    std::string out(static_cast<size_t>(len), '\0');
    WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), out.data(),
                        len, nullptr, nullptr);
    return out;
}

// Takes the frames `pending` completes out of it.
std::vector<std::string> Frames(std::vector<char>& pending) {
    std::vector<std::string> texts;
    for (;;) {
        bool at_magic = pending.size() >= sizeof(MAGIC) &&
                        memcmp(pending.data(), MAGIC, sizeof(MAGIC)) == 0;
        if (!at_magic) {
            size_t start = 1;
            while (start + sizeof(MAGIC) <= pending.size() &&
                   memcmp(pending.data() + start, MAGIC, sizeof(MAGIC)) != 0) {
                start++;
            }
            if (start + sizeof(MAGIC) > pending.size()) {
                // Keep what could be the start of a magic.
                size_t keep = pending.size() < sizeof(MAGIC) ? pending.size() : sizeof(MAGIC) - 1;
                pending.erase(pending.begin(), pending.end() - static_cast<ptrdiff_t>(keep));
                return texts;
            }
            pending.erase(pending.begin(), pending.begin() + static_cast<ptrdiff_t>(start));
        }
        if (pending.size() < HEADER) {
            return texts;
        }
        uint32_t len = 0;
        memcpy(&len, pending.data() + sizeof(MAGIC), sizeof(len));
        if (len > MAX_TEXT) {
            pending.erase(pending.begin());
            continue;
        }
        if (pending.size() < HEADER + len) {
            return texts;
        }
        texts.emplace_back(pending.data() + HEADER, len);
        pending.erase(pending.begin(), pending.begin() + static_cast<ptrdiff_t>(HEADER + len));
    }
}

// Reads frames from the host and hands them to the window, opening the port when it
// can.
DWORD WINAPI Reader(void* window) {
    OVERLAPPED overlapped = {};
    overlapped.hEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    std::vector<char> pending;
    char buffer[4096];
    for (;;) {
        if (g_port == INVALID_HANDLE_VALUE) {
            HANDLE port = OpenPort();
            if (port == INVALID_HANDLE_VALUE) {
                Sleep(RETRY_MS);
                continue;
            }
            g_port = port;
        }
        ResetEvent(overlapped.hEvent);
        DWORD read = 0;
        if (!ReadFile(g_port, buffer, sizeof(buffer), nullptr, &overlapped) &&
            GetLastError() != ERROR_IO_PENDING) {
            Sleep(RETRY_MS);
            continue;
        }
        if (!GetOverlappedResult(g_port, &overlapped, &read, TRUE)) {
            Sleep(RETRY_MS);
            continue;
        }
        pending.insert(pending.end(), buffer, buffer + read);
        for (const std::string& text : Frames(pending)) {
            auto wide = std::make_unique<std::wstring>(FromUtf8(text.data(), text.size()));
            if (PostMessageW(static_cast<HWND>(window), WM_FROM_HOST, 0,
                             reinterpret_cast<LPARAM>(wide.get()))) {
                wide.release();
            }
        }
    }
}

bool WriteAll(const std::string& bytes) {
    OVERLAPPED overlapped = {};
    overlapped.hEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    size_t done = 0;
    bool ok = true;
    while (done < bytes.size()) {
        DWORD written = 0;
        DWORD chunk = static_cast<DWORD>(bytes.size() - done < 65536 ? bytes.size() - done : 65536);
        ResetEvent(overlapped.hEvent);
        if (!WriteFile(g_port, bytes.data() + done, chunk, nullptr, &overlapped) &&
            GetLastError() != ERROR_IO_PENDING) {
            ok = false;
            break;
        }
        if (!GetOverlappedResult(g_port, &overlapped, &written, TRUE)) {
            ok = false;
            break;
        }
        done += written;
    }
    CloseHandle(overlapped.hEvent);
    return ok;
}

// Sends the latest text the user copied to the host.
DWORD WINAPI Writer(void*) {
    for (;;) {
        WaitForSingleObject(g_outgoing_ready, INFINITE);
        std::string text;
        EnterCriticalSection(&g_outgoing_lock);
        bool has = g_has_outgoing;
        text.swap(g_outgoing);
        g_has_outgoing = false;
        LeaveCriticalSection(&g_outgoing_lock);
        if (!has || g_port == INVALID_HANDLE_VALUE) {
            continue;
        }
        if (text.size() > MAX_TEXT) {
            // Cut at a character's end.
            size_t end = MAX_TEXT;
            while (end > 0 && (static_cast<unsigned char>(text[end]) & 0xc0) == 0x80) {
                end--;
            }
            text.resize(end);
        }
        uint32_t len = static_cast<uint32_t>(text.size());
        std::string frame(MAGIC, sizeof(MAGIC));
        frame.append(reinterpret_cast<const char*>(&len), sizeof(len));
        frame.append(text);
        WriteAll(frame);
    }
}

bool OpenClipboardPatiently(HWND window) {
    for (int i = 0; i < 20; i++) {
        if (OpenClipboard(window)) {
            return true;
        }
        Sleep(25);
    }
    return false;
}

void SendClipboard(HWND window) {
    if (!IsClipboardFormatAvailable(CF_UNICODETEXT) || !OpenClipboardPatiently(window)) {
        return;
    }
    std::wstring text;
    if (HANDLE data = GetClipboardData(CF_UNICODETEXT)) {
        if (const auto* chars = static_cast<const wchar_t*>(GlobalLock(data))) {
            text.assign(chars, wcsnlen(chars, GlobalSize(data) / sizeof(wchar_t)));
            GlobalUnlock(data);
        }
    }
    CloseClipboard();
    EnterCriticalSection(&g_outgoing_lock);
    g_outgoing = ToUtf8(text);
    g_has_outgoing = true;
    LeaveCriticalSection(&g_outgoing_lock);
    SetEvent(g_outgoing_ready);
}

void SetClipboard(HWND window, const std::wstring& text) {
    if (!OpenClipboardPatiently(window)) {
        return;
    }
    EmptyClipboard();
    HGLOBAL data = GlobalAlloc(GMEM_MOVEABLE, (text.size() + 1) * sizeof(wchar_t));
    if (data != nullptr) {
        if (auto* chars = static_cast<wchar_t*>(GlobalLock(data))) {
            memcpy(chars, text.c_str(), (text.size() + 1) * sizeof(wchar_t));
            GlobalUnlock(data);
            if (SetClipboardData(CF_UNICODETEXT, data) == nullptr) {
                GlobalFree(data);
            }
        } else {
            GlobalFree(data);
        }
    }
    CloseClipboard();
    g_own_sequence = GetClipboardSequenceNumber();
}

LRESULT CALLBACK WindowProc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    switch (message) {
        case WM_CLIPBOARDUPDATE:
            if (GetClipboardSequenceNumber() != g_own_sequence) {
                SetTimer(window, READ_TIMER, READ_DELAY_MS, nullptr);
            }
            return 0;
        case WM_TIMER:
            if (wparam == READ_TIMER) {
                KillTimer(window, READ_TIMER);
                if (GetClipboardSequenceNumber() != g_own_sequence) {
                    SendClipboard(window);
                }
            }
            return 0;
        case WM_FROM_HOST: {
            std::unique_ptr<std::wstring> text(reinterpret_cast<std::wstring*>(lparam));
            SetClipboard(window, *text);
            return 0;
        }
        default:
            return DefWindowProcW(window, message, wparam, lparam);
    }
}

}  // namespace

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR, int) {
    // One per session.
    HANDLE single = CreateMutexW(nullptr, TRUE, L"Local\\vmmcagent");
    if (single == nullptr || GetLastError() == ERROR_ALREADY_EXISTS) {
        return 0;
    }
    InitializeCriticalSection(&g_outgoing_lock);
    g_outgoing_ready = CreateEventW(nullptr, FALSE, FALSE, nullptr);

    WNDCLASSW window_class = {};
    window_class.lpfnWndProc = WindowProc;
    window_class.hInstance = instance;
    window_class.lpszClassName = L"vmmcagent";
    RegisterClassW(&window_class);
    HWND window = CreateWindowExW(0, L"vmmcagent", L"vmmc agent", 0, 0, 0, 0, 0, HWND_MESSAGE,
                                  nullptr, instance, nullptr);
    if (window == nullptr || !AddClipboardFormatListener(window)) {
        return 1;
    }
    CreateThread(nullptr, 0, Reader, window, 0, nullptr);
    CreateThread(nullptr, 0, Writer, nullptr, 0, nullptr);

    MSG message;
    while (GetMessageW(&message, nullptr, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return 0;
}
