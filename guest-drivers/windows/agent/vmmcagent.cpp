// vmmc agent (vmmcagent.exe): vmmc's helper in a Windows guest. It shares the
// clipboard's text with the host, and drags files between the host and the guest.
//
// - The control line is the guest's second serial port (COM2), where vmmc's worker
//   listens. The clipboard's text goes in "\x1bVCLIP" frames (a length, then UTF-8);
//   everything else in "\x1bVAGNT" message frames. The agent_proto crate has the
//   protocol and the messages' encoding, which this file follows.
// - A drag's files go over TCP instead, to the address the worker names: its NAT answers
//   it itself. A serial port would take minutes over a few megabytes.
// - A drag from the host: the worker moves the pointer where the host's drag is and says
//   so. The agent stages the items in %TEMP%\vmmc, puts a small invisible window under
//   the pointer, and the worker's button press on it starts a drag (DoDragDrop) of the
//   staged items, which the host's drag then steers. The worker lets the button go once
//   the files are all there, and the guest drops them wherever the pointer is.
// - A drag out to the host: when the guest's button is held and the host's pointer has
//   left the console, the worker asks. The agent shows an invisible window over the
//   whole screen that takes drops; the guest's drag, nudged onto it, tells the agent
//   which files it carries. The agent refuses the drop, and sends the files when asked.
//
// The vmmc Tools setup installs it in Program Files and starts it at each logon (the
// HKLM Run key). It needs no driver, so it works with Secure Boot on. One copy runs per
// session; the first to open COM2 has it.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <ole2.h>
#include <shellapi.h>
#include <shlobj.h>

#include <atomic>
#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cwchar>
#include <cwctype>
#include <deque>
#include <memory>
#include <string>
#include <vector>

namespace {

// --- The protocol (agent_proto) ------------------------------------------------------

constexpr char CLIP_MAGIC[6] = {0x1b, 'V', 'C', 'L', 'I', 'P'};
constexpr char MSG_MAGIC[6] = {0x1b, 'V', 'A', 'G', 'N', 'T'};
constexpr size_t HEADER = 10;
constexpr size_t MAX_TEXT = 512 * 1024;
constexpr size_t MAX_MESSAGE = 1 << 20;
constexpr size_t MAX_DATA = 256 * 1024;
constexpr uint32_t PROTOCOL_VERSION = 1;
constexpr uint32_t FEATURES = 1 | 2 | 4;  // clipboard, drags in, drags out

enum Kind : uint8_t {
    HELLO = 1,
    LOG = 2,
    DRAG_ARMED = 3,
    DRAG_FINISHED = 4,
    GUEST_DRAG = 5,
    NO_GUEST_DRAG = 6,
    PING = 32,
    CONFIG = 33,
    HOST_DRAG = 34,
    CANCEL_DRAG = 35,
    PROBE = 36,
    SEND_FILES = 37,
    CANCEL_TRANSFER = 38,
    ATTACH = 64,
    CHUNK = 65,
    RECEIVED = 66,
};

enum ChunkKind : uint8_t { TOTAL = 1, BEGIN = 2, DATA = 3, END = 4, FINISH = 5 };

struct Item {
    std::string name;
    bool dir = false;
};

// A decoded message: the fields its kind has.
struct Message {
    uint8_t kind = 0;
    uint32_t id = 0;      // drag, probe or transfer
    uint32_t second = 0;  // SendFiles' transfer
    std::vector<Item> items;
    uint8_t address[4] = {};
    uint16_t port = 0;
    uint8_t token[16] = {};
    uint8_t chunk = 0;
    std::string text;  // a path, or an error
    bool has_text = false;
    bool flag = false;  // a folder
    uint64_t size = 0;
    std::string data;
};

class Writer {
public:
    std::string out;
    void u8(uint8_t v) { out.push_back(static_cast<char>(v)); }
    void u16(uint16_t v) { out.append(reinterpret_cast<const char*>(&v), 2); }
    void u32(uint32_t v) { out.append(reinterpret_cast<const char*>(&v), 4); }
    void u64(uint64_t v) { out.append(reinterpret_cast<const char*>(&v), 8); }
    void raw(const void* p, size_t n) { out.append(static_cast<const char*>(p), n); }
    void bytes(const char* p, size_t n) {
        u32(static_cast<uint32_t>(n));
        raw(p, n);
    }
    void str(const std::string& s) { bytes(s.data(), s.size()); }
    void optional_str(const std::string* s) {
        if (s == nullptr) {
            u8(0);
        } else {
            u8(1);
            str(*s);
        }
    }
    void items(const std::vector<Item>& items) {
        u32(static_cast<uint32_t>(items.size()));
        for (const Item& item : items) {
            str(item.name);
            u8(item.dir ? 1 : 0);
        }
    }
    // The message as a frame.
    std::string frame() const {
        std::string frame(MSG_MAGIC, sizeof(MSG_MAGIC));
        uint32_t len = static_cast<uint32_t>(out.size());
        frame.append(reinterpret_cast<const char*>(&len), 4);
        frame.append(out);
        return frame;
    }
};

class Reader {
public:
    Reader(const char* p, size_t n) : p_(p), n_(n) {}
    bool take(void* out, size_t n) {
        if (n_ < n) return false;
        memcpy(out, p_, n);
        p_ += n;
        n_ -= n;
        return true;
    }
    bool u8(uint8_t& v) { return take(&v, 1); }
    bool flag(bool& v) {
        uint8_t b;
        if (!u8(b) || b > 1) return false;
        v = b == 1;
        return true;
    }
    bool u16(uint16_t& v) { return take(&v, 2); }
    bool u32(uint32_t& v) { return take(&v, 4); }
    bool u64(uint64_t& v) { return take(&v, 8); }
    bool bytes(std::string& s) {
        uint32_t len;
        if (!u32(len) || n_ < len) return false;
        s.assign(p_, len);
        p_ += len;
        n_ -= len;
        return true;
    }
    bool optional_str(std::string& s, bool& has) {
        uint8_t b;
        if (!u8(b) || b > 1) return false;
        has = b == 1;
        return !has || bytes(s);
    }
    bool items(std::vector<Item>& items) {
        uint32_t count;
        if (!u32(count) || count > 65536) return false;
        for (uint32_t i = 0; i < count; i++) {
            Item item;
            if (!bytes(item.name) || !flag(item.dir)) return false;
            items.push_back(std::move(item));
        }
        return true;
    }

private:
    const char* p_;
    size_t n_;
};

bool Decode(const std::string& body, Message& m) {
    Reader r(body.data(), body.size());
    if (!r.u8(m.kind)) return false;
    switch (m.kind) {
        case PING:
            return true;
        case CONFIG:
            return r.take(m.address, 4) && r.u16(m.port) && r.take(m.token, 16);
        case HOST_DRAG: {
            uint64_t files, bytes;
            return r.u32(m.id) && r.items(m.items) && r.u64(files) && r.u64(bytes);
        }
        case CANCEL_DRAG:
        case PROBE:
        case CANCEL_TRANSFER:
            return r.u32(m.id);
        case SEND_FILES:
            return r.u32(m.id) && r.u32(m.second);
        case CHUNK:
            if (!r.u32(m.id) || !r.u8(m.chunk)) return false;
            switch (m.chunk) {
                case TOTAL: {
                    uint64_t files, bytes;
                    return r.u64(files) && r.u64(bytes);
                }
                case BEGIN:
                    return r.bytes(m.text) && r.flag(m.flag) && r.u64(m.size);
                case DATA:
                    return r.bytes(m.data) && m.data.size() <= MAX_DATA;
                case END:
                    return true;
                case FINISH:
                    return r.optional_str(m.text, m.has_text);
                default:
                    return false;
            }
        default:
            // Not for the agent, or from a later version.
            return false;
    }
}

// What comes off the control line or a data connection.
struct Frame {
    bool clipboard;
    std::string body;
};

// Takes frames of both kinds out of a byte stream.
class Parser {
public:
    std::vector<Frame> Feed(const char* bytes, size_t n) {
        pending_.insert(pending_.end(), bytes, bytes + n);
        std::vector<Frame> frames;
        for (;;) {
            size_t at = start_;
            for (; at < pending_.size(); at++) {
                size_t left = pending_.size() - at;
                size_t k = left < 6 ? left : 6;
                if (memcmp(&pending_[at], MSG_MAGIC, k) == 0 || memcmp(&pending_[at], CLIP_MAGIC, k) == 0) {
                    break;
                }
            }
            start_ = at;
            if (pending_.size() - start_ < HEADER) break;
            const char* rest = &pending_[start_];
            bool message = memcmp(rest, MSG_MAGIC, 6) == 0;
            if (!message && memcmp(rest, CLIP_MAGIC, 6) != 0) {
                start_++;
                continue;
            }
            uint32_t len;
            memcpy(&len, rest + 6, 4);
            if (len > (message ? MAX_MESSAGE : MAX_TEXT)) {
                start_++;
                continue;
            }
            if (pending_.size() - start_ < HEADER + len) break;
            frames.push_back(Frame{!message, std::string(rest + HEADER, len)});
            start_ += HEADER + len;
        }
        if (start_ > 0 && (start_ == pending_.size() || start_ >= 65536)) {
            pending_.erase(pending_.begin(), pending_.begin() + static_cast<ptrdiff_t>(start_));
            start_ = 0;
        }
        return frames;
    }

private:
    std::vector<char> pending_;
    size_t start_ = 0;
};

// --- Text ------------------------------------------------------------------------------

std::wstring FromUtf8(const std::string& text) {
    if (text.empty()) return {};
    int wide = MultiByteToWideChar(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), nullptr, 0);
    std::wstring out(static_cast<size_t>(wide), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), out.data(), wide);
    return out;
}

std::string ToUtf8(const std::wstring& text) {
    if (text.empty()) return {};
    int len = WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), nullptr, 0,
                                  nullptr, nullptr);
    std::string out(static_cast<size_t>(len), '\0');
    WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), out.data(), len, nullptr,
                        nullptr);
    return out;
}

// --- The control line ------------------------------------------------------------------

// Messages for the window to act on: lParam is a Message*, or (WM_FROM_HOST) the
// clipboard's text as a std::wstring*.
constexpr UINT WM_FROM_HOST = WM_APP + 1;
constexpr UINT WM_MESSAGE = WM_APP + 2;
constexpr DWORD RETRY_MS = 5000;

HANDLE g_port = INVALID_HANDLE_VALUE;
HWND g_window = nullptr;
// The host drag being called off: QueryContinueDrag ends it.
std::atomic<uint32_t> g_cancelled_drag{0};

// What the writer sends: messages in order, and the latest clipboard text.
CRITICAL_SECTION g_outgoing_lock;
std::deque<std::string> g_outgoing_frames;
std::string g_outgoing_text;
bool g_has_outgoing_text = false;
HANDLE g_outgoing_ready = nullptr;

HANDLE OpenPort() {
    HANDLE port = CreateFileW(L"\\\\.\\COM2", GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
                              FILE_FLAG_OVERLAPPED, nullptr);
    if (port == INVALID_HANDLE_VALUE) return port;
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

void Send(const Writer& message) {
    EnterCriticalSection(&g_outgoing_lock);
    g_outgoing_frames.push_back(message.frame());
    LeaveCriticalSection(&g_outgoing_lock);
    SetEvent(g_outgoing_ready);
}

// A line for the worker's log.
void Log(const char* format, ...) {
    char line[512];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof(line), format, args);
    va_end(args);
    OutputDebugStringA(line);
    Writer w;
    w.u8(LOG);
    w.str(line);
    Send(w);
}

void Hello() {
    Writer w;
    w.u8(HELLO);
    w.u32(PROTOCOL_VERSION);
    w.str("windows");
    w.u32(FEATURES);
    Send(w);
}

// How often the agent says hello: the worker sends nothing until it has heard one (the
// firmware reads COM2 as a keyboard), and a worker started anew, as a resume does,
// hasn't.
constexpr ULONGLONG HELLO_INTERVAL_MS = 30000;

// Reads frames from the host and hands them to the window, opening the port when it can.
DWORD WINAPI PortReader(void*) {
    OVERLAPPED overlapped = {};
    overlapped.hEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    Parser parser;
    char buffer[4096];
    ULONGLONG said_hello = 0;
    for (;;) {
        if (g_port == INVALID_HANDLE_VALUE) {
            HANDLE port = OpenPort();
            if (port == INVALID_HANDLE_VALUE) {
                Sleep(RETRY_MS);
                continue;
            }
            g_port = port;
            said_hello = 0;
        }
        if (said_hello == 0 || GetTickCount64() - said_hello > HELLO_INTERVAL_MS) {
            Hello();
            said_hello = GetTickCount64();
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
        for (Frame& frame : parser.Feed(buffer, read)) {
            if (frame.clipboard) {
                auto wide = std::make_unique<std::wstring>(FromUtf8(frame.body));
                if (PostMessageW(g_window, WM_FROM_HOST, 0, reinterpret_cast<LPARAM>(wide.get()))) {
                    wide.release();
                }
                continue;
            }
            auto message = std::make_unique<Message>();
            if (!Decode(frame.body, *message)) continue;
            if (message->kind == PING) {
                Hello();
                continue;
            }
            if (message->kind == CANCEL_DRAG) {
                // Here, not on the window's thread: a drag's loop (DoDragDrop) runs there
                // and leaves posted messages waiting, and it's that drag being called off.
                g_cancelled_drag = message->id;
            }
            if (PostMessageW(g_window, WM_MESSAGE, 0, reinterpret_cast<LPARAM>(message.get()))) {
                message.release();
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

// Sends messages, and the latest text the user copied, to the host.
DWORD WINAPI PortWriter(void*) {
    for (;;) {
        WaitForSingleObject(g_outgoing_ready, INFINITE);
        for (;;) {
            std::string frame;
            bool has_frame = false;
            std::string text;
            bool has_text = false;
            EnterCriticalSection(&g_outgoing_lock);
            if (!g_outgoing_frames.empty()) {
                frame.swap(g_outgoing_frames.front());
                g_outgoing_frames.pop_front();
                has_frame = true;
            } else if (g_has_outgoing_text) {
                text.swap(g_outgoing_text);
                g_has_outgoing_text = false;
                has_text = true;
            }
            LeaveCriticalSection(&g_outgoing_lock);
            if (!has_frame && !has_text) break;
            if (g_port == INVALID_HANDLE_VALUE) continue;
            if (has_text) {
                if (text.size() > MAX_TEXT) {
                    // Cut at a character's end.
                    size_t end = MAX_TEXT;
                    while (end > 0 && (static_cast<unsigned char>(text[end]) & 0xc0) == 0x80) end--;
                    text.resize(end);
                }
                uint32_t len = static_cast<uint32_t>(text.size());
                frame.assign(CLIP_MAGIC, sizeof(CLIP_MAGIC));
                frame.append(reinterpret_cast<const char*>(&len), sizeof(len));
                frame.append(text);
            }
            WriteAll(frame);
        }
    }
}

// --- The clipboard ---------------------------------------------------------------------

// The clipboard is read this long after it changes, not at once: the program that set it
// may still have it open (to flush it, say), and a burst of changes is read once.
constexpr UINT_PTR READ_TIMER = 1;
constexpr UINT READ_DELAY_MS = 150;

// The clipboard's sequence number after the agent last set it: that change isn't the
// user's, and doesn't go back to the host.
DWORD g_own_sequence = 0;

bool OpenClipboardPatiently(HWND window) {
    for (int i = 0; i < 20; i++) {
        if (OpenClipboard(window)) return true;
        Sleep(25);
    }
    return false;
}

void SendClipboard(HWND window) {
    if (!IsClipboardFormatAvailable(CF_UNICODETEXT) || !OpenClipboardPatiently(window)) return;
    std::wstring text;
    if (HANDLE data = GetClipboardData(CF_UNICODETEXT)) {
        if (const auto* chars = static_cast<const wchar_t*>(GlobalLock(data))) {
            text.assign(chars, wcsnlen(chars, GlobalSize(data) / sizeof(wchar_t)));
            GlobalUnlock(data);
        }
    }
    CloseClipboard();
    EnterCriticalSection(&g_outgoing_lock);
    g_outgoing_text = ToUtf8(text);
    g_has_outgoing_text = true;
    LeaveCriticalSection(&g_outgoing_lock);
    SetEvent(g_outgoing_ready);
}

void SetClipboard(HWND window, const std::wstring& text) {
    if (!OpenClipboardPatiently(window)) return;
    EmptyClipboard();
    HGLOBAL data = GlobalAlloc(GMEM_MOVEABLE, (text.size() + 1) * sizeof(wchar_t));
    if (data != nullptr) {
        if (auto* chars = static_cast<wchar_t*>(GlobalLock(data))) {
            memcpy(chars, text.c_str(), (text.size() + 1) * sizeof(wchar_t));
            GlobalUnlock(data);
            if (SetClipboardData(CF_UNICODETEXT, data) == nullptr) GlobalFree(data);
        } else {
            GlobalFree(data);
        }
    }
    CloseClipboard();
    g_own_sequence = GetClipboardSequenceNumber();
}

// --- Files -----------------------------------------------------------------------------

// Where data connections go, once the worker has said (Config).
CRITICAL_SECTION g_config_lock;
bool g_has_config = false;
sockaddr_in g_data_address = {};
uint8_t g_token[16] = {};

// Whether `name` is a plain name a Windows folder can hold (agent_proto::path).
bool GoodName(const std::wstring& name) {
    if (name.empty() || name == L"." || name == L".." || name.size() > 255) return false;
    for (wchar_t c : name) {
        if (c < 0x20 || c == 0x7f || wcschr(L"<>:\"/\\|?*", c) != nullptr) return false;
    }
    if (name.back() == L'.' || name.back() == L' ') return false;
    std::wstring stem = name.substr(0, name.find(L'.'));
    while (!stem.empty() && stem.back() == L' ') stem.pop_back();
    for (wchar_t& c : stem) c = static_cast<wchar_t>(towupper(c));
    for (const wchar_t* device : {L"CON", L"PRN", L"AUX", L"NUL", L"CONIN$", L"CONOUT$"}) {
        if (stem == device) return false;
    }
    if (stem.size() == 4 && (stem.compare(0, 3, L"COM") == 0 || stem.compare(0, 3, L"LPT") == 0)) {
        wchar_t d = stem[3];
        // Digits, and superscript 1, 2 and 3.
        if ((d >= L'0' && d <= L'9') || d == 0xb9 || d == 0xb2 || d == 0xb3) return false;
    }
    return true;
}

// `path` from a transfer ("a/b/c") as a Windows path under `root`, if every name in it
// is a good one.
bool SafePath(const std::wstring& root, const std::string& path, std::wstring& out) {
    std::wstring wide = FromUtf8(path);
    out = root;
    size_t start = 0;
    int depth = 0;
    for (;;) {
        size_t slash = wide.find(L'/', start);
        std::wstring name = wide.substr(start, slash == std::wstring::npos ? std::wstring::npos : slash - start);
        if (!GoodName(name) || ++depth > 64) return false;
        out += L"\\";
        out += name;
        if (slash == std::wstring::npos) return true;
        start = slash + 1;
    }
}

// `path` with the prefix that lifts Windows' 260-character limit.
std::wstring Long(const std::wstring& path) {
    if (path.compare(0, 4, L"\\\\?\\") == 0) return path;
    return L"\\\\?\\" + path;
}

void DeleteTree(const std::wstring& path) {
    WIN32_FIND_DATAW found;
    HANDLE find = FindFirstFileW(Long(path + L"\\*").c_str(), &found);
    if (find != INVALID_HANDLE_VALUE) {
        do {
            std::wstring name = found.cFileName;
            if (name == L"." || name == L"..") continue;
            std::wstring child = path + L"\\" + name;
            if ((found.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) &&
                !(found.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
                DeleteTree(child);
            } else {
                SetFileAttributesW(Long(child).c_str(), FILE_ATTRIBUTE_NORMAL);
                DeleteFileW(Long(child).c_str());
                RemoveDirectoryW(Long(child).c_str());
            }
        } while (FindNextFileW(find, &found));
        FindClose(find);
    }
    RemoveDirectoryW(Long(path).c_str());
}

// %TEMP%\vmmc, where drags from the host are staged.
std::wstring StagingRoot() {
    wchar_t temp[MAX_PATH];
    DWORD len = GetTempPathW(MAX_PATH, temp);
    std::wstring root(temp, len);
    if (!root.empty() && root.back() == L'\\') root.pop_back();
    root += L"\\vmmc";
    CreateDirectoryW(root.c_str(), nullptr);
    return root;
}

// Removes staged drags: all of them, or those older than an hour (a program may still
// be copying from a recent one).
void CleanStaging(bool all) {
    std::wstring root = StagingRoot();
    WIN32_FIND_DATAW found;
    HANDLE find = FindFirstFileW((root + L"\\drop-*").c_str(), &found);
    if (find == INVALID_HANDLE_VALUE) return;
    FILETIME now_time;
    GetSystemTimeAsFileTime(&now_time);
    ULARGE_INTEGER now = {{now_time.dwLowDateTime, now_time.dwHighDateTime}};
    do {
        ULARGE_INTEGER made = {{found.ftCreationTime.dwLowDateTime, found.ftCreationTime.dwHighDateTime}};
        bool old = now.QuadPart - made.QuadPart > 3600ull * 10000000ull;
        if (all || old) DeleteTree(root + L"\\" + found.cFileName);
    } while (FindNextFileW(find, &found));
    FindClose(find);
}

// A data connection to the worker for transfer `transfer`, or INVALID_SOCKET.
SOCKET Connect(uint32_t transfer) {
    EnterCriticalSection(&g_config_lock);
    bool has = g_has_config;
    sockaddr_in address = g_data_address;
    uint8_t token[16];
    memcpy(token, g_token, sizeof(token));
    LeaveCriticalSection(&g_config_lock);
    if (!has) {
        Log("no data address from the worker yet");
        return INVALID_SOCKET;
    }
    SOCKET s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (s == INVALID_SOCKET) return s;
    BOOL nodelay = TRUE;
    setsockopt(s, IPPROTO_TCP, TCP_NODELAY, reinterpret_cast<const char*>(&nodelay), sizeof(nodelay));
    if (connect(s, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) {
        Log("can't connect to the worker for files: error %d (is the VM's network adapter on?)",
            WSAGetLastError());
        closesocket(s);
        return INVALID_SOCKET;
    }
    Writer w;
    w.u8(ATTACH);
    w.raw(token, sizeof(token));
    w.u32(transfer);
    std::string frame = w.frame();
    if (send(s, frame.data(), static_cast<int>(frame.size()), 0) != static_cast<int>(frame.size())) {
        closesocket(s);
        return INVALID_SOCKET;
    }
    return s;
}

bool SendAll(SOCKET s, const std::string& bytes) {
    size_t done = 0;
    while (done < bytes.size()) {
        int n = send(s, bytes.data() + done, static_cast<int>(bytes.size() - done), 0);
        if (n <= 0) return false;
        done += static_cast<size_t>(n);
    }
    return true;
}

Writer ChunkMessage(uint32_t transfer, uint8_t kind) {
    Writer w;
    w.u8(CHUNK);
    w.u32(transfer);
    w.u8(kind);
    return w;
}

// --- A drag from the host --------------------------------------------------------------

struct HostDrag {
    uint32_t drag = 0;
    std::wstring staging;
    std::vector<std::wstring> items;  // the staged items' paths
    HWND window = nullptr;
    SOCKET socket = INVALID_SOCKET;
    bool started = false;  // DoDragDrop runs
    ULONGLONG pressed_at = 0;  // when the press timer first saw the button down
};

std::unique_ptr<HostDrag> g_host_drag;
constexpr UINT_PTR ARM_TIMER = 2;
constexpr UINT ARM_TIMEOUT_MS = 5000;
// Says the drag is armed, a moment after the window shows.
constexpr UINT_PTR ARMED_TIMER = 5;
constexpr UINT ARMED_DELAY_MS = 40;
// Looks for the worker's button press, in case its click doesn't reach the window.
constexpr UINT_PTR PRESS_TIMER = 4;
constexpr UINT PRESS_POLL_MS = 30;
// How long a press may go without its click before the drag starts anyway.
constexpr ULONGLONG CLICK_GRACE_MS = 100;

void SendDragFinished(uint32_t drag, bool dropped) {
    Writer w;
    w.u8(DRAG_FINISHED);
    w.u32(drag);
    w.u8(dropped ? 1 : 0);
    Send(w);
}

void EndHostDrag(bool dropped) {
    if (!g_host_drag) return;
    std::unique_ptr<HostDrag> drag = std::move(g_host_drag);
    if (drag->window != nullptr) DestroyWindow(drag->window);
    if (drag->socket != INVALID_SOCKET) {
        // The receiver thread sees it and stops.
        shutdown(drag->socket, SD_BOTH);
    }
    if (!dropped) DeleteTree(drag->staging);
    SendDragFinished(drag->drag, dropped);
}

struct ReceiveJob {
    uint32_t transfer;
    std::wstring staging;
    SOCKET socket;
};

void SendReceived(SOCKET s, uint32_t transfer, const std::string* error) {
    Writer w;
    w.u8(RECEIVED);
    w.u32(transfer);
    w.optional_str(error);
    SendAll(s, w.frame());
}

// Receives a drag's files into its staging folder, then says so.
DWORD WINAPI Receive(void* param) {
    std::unique_ptr<ReceiveJob> job(static_cast<ReceiveJob*>(param));
    SOCKET s = job->socket;
    Parser parser;
    std::vector<char> buffer(64 * 1024);
    HANDLE file = INVALID_HANDLE_VALUE;
    uint64_t expected = 0, written = 0, files = 0, bytes = 0;
    std::string error;
    bool finished = false;
    while (!finished && error.empty()) {
        int n = recv(s, buffer.data(), static_cast<int>(buffer.size()), 0);
        if (n <= 0) {
            error = "the connection closed early";
            break;
        }
        for (Frame& frame : parser.Feed(buffer.data(), static_cast<size_t>(n))) {
            Message m;
            if (frame.clipboard || !Decode(frame.body, m) || m.kind != CHUNK || m.id != job->transfer) continue;
            if (m.chunk == BEGIN) {
                if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
                file = INVALID_HANDLE_VALUE;
                std::wstring path;
                if (!SafePath(job->staging, m.text, path)) {
                    error = "a bad name: " + m.text;
                    break;
                }
                if (m.flag) {
                    if (!CreateDirectoryW(Long(path).c_str(), nullptr) && GetLastError() != ERROR_ALREADY_EXISTS) {
                        error = "can't make the folder " + m.text;
                    }
                } else {
                    file = CreateFileW(Long(path).c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                                       FILE_ATTRIBUTE_NORMAL, nullptr);
                    if (file == INVALID_HANDLE_VALUE) error = "can't make " + m.text;
                    expected = m.size;
                    written = 0;
                    files++;
                }
            } else if (m.chunk == DATA) {
                DWORD done = 0;
                written += m.data.size();
                bytes += m.data.size();
                if (file == INVALID_HANDLE_VALUE || written > expected ||
                    !WriteFile(file, m.data.data(), static_cast<DWORD>(m.data.size()), &done, nullptr)) {
                    error = "can't write a file";
                }
            } else if (m.chunk == END) {
                if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
                file = INVALID_HANDLE_VALUE;
            } else if (m.chunk == FINISH) {
                if (m.has_text) error = m.text;
                finished = true;
            }
            if (finished || !error.empty()) break;
        }
    }
    if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    if (error.empty()) {
        Log("received %llu files (%llu bytes) from the host", files, bytes);
        SendReceived(s, job->transfer, nullptr);
    } else {
        Log("receiving files from the host failed: %s", error.c_str());
        SendReceived(s, job->transfer, &error);
    }
    shutdown(s, SD_SEND);
    // Waits for the worker to close, so the receipt gets there.
    char drain[256];
    while (recv(s, drain, sizeof(drain), 0) > 0) {
    }
    closesocket(s);
    return 0;
}

class DropSource : public IDropSource {
public:
    explicit DropSource(uint32_t drag) : drag_(drag) {}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** out) override {
        if (riid == IID_IUnknown || riid == IID_IDropSource) {
            *out = static_cast<IDropSource*>(this);
            AddRef();
            return S_OK;
        }
        *out = nullptr;
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++refs_; }
    ULONG STDMETHODCALLTYPE Release() override {
        ULONG left = --refs_;
        if (left == 0) delete this;
        return left;
    }
    HRESULT STDMETHODCALLTYPE QueryContinueDrag(BOOL escape, DWORD keys) override {
        if (escape || g_cancelled_drag.load() == drag_) return DRAGDROP_S_CANCEL;
        if (!(keys & MK_LBUTTON)) return DRAGDROP_S_DROP;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GiveFeedback(DWORD) override { return DRAGDROP_S_USEDEFAULTCURSORS; }

private:
    std::atomic<ULONG> refs_{1};
    uint32_t drag_;
};

// A shell data object for the staged items, as Explorer makes for its own drags.
IDataObject* StagedDataObject(const HostDrag& drag) {
    PIDLIST_ABSOLUTE folder = ILCreateFromPathW(drag.staging.c_str());
    if (folder == nullptr) return nullptr;
    std::vector<PIDLIST_ABSOLUTE> full;
    std::vector<PCUITEMID_CHILD> children;
    for (const std::wstring& item : drag.items) {
        PIDLIST_ABSOLUTE pidl = ILCreateFromPathW(item.c_str());
        if (pidl == nullptr) continue;
        full.push_back(pidl);
        children.push_back(ILFindLastID(pidl));
    }
    IDataObject* object = nullptr;
    if (!children.empty()) {
        SHCreateDataObject(folder, static_cast<UINT>(children.size()), children.data(), nullptr,
                           IID_PPV_ARGS(&object));
    }
    for (PIDLIST_ABSOLUTE pidl : full) ILFree(pidl);
    ILFree(folder);
    return object;
}

// The window under the pointer, the foreground window, and which window has the mouse.
void LogInput(const char* when) {
    POINT at;
    GetCursorPos(&at);
    GUITHREADINFO info = {};
    info.cbSize = sizeof(info);
    GetGUIThreadInfo(0, &info);
    Log("%s: pointer %ld,%ld, under it %p, foreground %p, capture %p, left button %s", when, at.x, at.y,
        static_cast<void*>(WindowFromPoint(at)), static_cast<void*>(GetForegroundWindow()),
        static_cast<void*>(info.hwndCapture), (GetAsyncKeyState(VK_LBUTTON) & 0x8000) ? "down" : "up");
}

// The worker pressed the button: the guest's drag of the staged items starts, and runs
// until it drops or is called off.
void RunHostDrag(HWND window) {
    if (!g_host_drag || g_host_drag->window != window || g_host_drag->started) return;
    KillTimer(window, ARM_TIMER);
    KillTimer(window, PRESS_TIMER);
    g_host_drag->started = true;
    ShowWindow(window, SW_HIDE);
    uint32_t drag = g_host_drag->drag;
    IDataObject* object = StagedDataObject(*g_host_drag);
    if (object == nullptr) {
        Log("can't make a data object for the staged files");
        EndHostDrag(false);
        return;
    }
    auto* source = new DropSource(drag);
    DWORD effect = DROPEFFECT_NONE;
    Log("dragging the host's files in the guest");
    HRESULT hr = DoDragDrop(object, source, DROPEFFECT_COPY | DROPEFFECT_MOVE, &effect);
    source->Release();
    object->Release();
    bool dropped = hr == DRAGDROP_S_DROP;
    Log("the drag ended: %s", dropped ? "dropped" : "called off");
    // A later drag may have replaced this one meanwhile.
    if (g_host_drag && g_host_drag->drag == drag) EndHostDrag(dropped);
}

LRESULT CALLBACK HelperProc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    switch (message) {
        case WM_MOUSEACTIVATE:
            return MA_NOACTIVATE;
        case WM_LBUTTONDOWN:
            RunHostDrag(window);
            return 0;
        case WM_TIMER:
            if (wparam == ARMED_TIMER) {
                KillTimer(window, ARMED_TIMER);
                if (g_host_drag && g_host_drag->window == window && !g_host_drag->started) {
                    Writer w;
                    w.u8(DRAG_ARMED);
                    w.u32(g_host_drag->drag);
                    Send(w);
                }
                return 0;
            }
            if (wparam == PRESS_TIMER) {
                if (!g_host_drag || g_host_drag->window != window || g_host_drag->started) {
                    KillTimer(window, PRESS_TIMER);
                    return 0;
                }
                if (!(GetAsyncKeyState(VK_LBUTTON) & 0x8000)) return 0;
                ULONGLONG now = GetTickCount64();
                if (g_host_drag->pressed_at == 0) {
                    g_host_drag->pressed_at = now;
                } else if (now - g_host_drag->pressed_at > CLICK_GRACE_MS) {
                    // The button is down, but its click went elsewhere. (Starting the
                    // drag regardless doesn't work: its loop never sees the pointer.)
                    KillTimer(window, PRESS_TIMER);
                    LogInput("the button is down, but its click didn't come here");
                }
                return 0;
            }
            if (wparam == ARM_TIMER) {
                KillTimer(window, ARM_TIMER);
                if (g_host_drag && g_host_drag->window == window && !g_host_drag->started) {
                    Log("the worker never pressed the button");
                    EndHostDrag(false);
                }
            }
            return 0;
        default:
            return DefWindowProcW(window, message, wparam, lparam);
    }
}

// An invisible window over the whole screen, for the worker's button press to start the
// guest's drag on. (Not just under the pointer: Windows may not have moved the pointer
// yet when the worker's message comes.)
HWND HelperWindow() {
    int x = GetSystemMetrics(SM_XVIRTUALSCREEN), y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    int width = GetSystemMetrics(SM_CXVIRTUALSCREEN), height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    HWND window = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                                  L"vmmcagent-helper", L"vmmc drag", WS_POPUP, x, y, width, height, nullptr,
                                  nullptr, GetModuleHandleW(nullptr), nullptr);
    if (window == nullptr) return nullptr;
    // Not quite clear: a fully clear layered window lets clicks through.
    SetLayeredWindowAttributes(window, 0, 1, LWA_ALPHA);
    ShowWindow(window, SW_SHOWNOACTIVATE);
    SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    // Painted now: until a layered window has painted, clicks go through it.
    UpdateWindow(window);
    return window;
}

void StartHostDrag(const Message& m) {
    if (g_host_drag) {
        if (g_host_drag->started) {
            // DoDragDrop runs further up this thread's stack: let it end first.
            g_cancelled_drag = g_host_drag->drag;
        }
        EndHostDrag(false);
    }
    CleanStaging(false);
    auto drag = std::make_unique<HostDrag>();
    drag->drag = m.id;
    wchar_t name[64];
    swprintf(name, 64, L"\\drop-%u-%llu", m.id, static_cast<unsigned long long>(GetTickCount64()));
    drag->staging = StagingRoot() + name;
    if (!CreateDirectoryW(drag->staging.c_str(), nullptr)) {
        Log("can't make the staging folder");
        SendDragFinished(m.id, false);
        return;
    }
    // The items, there already for the drag's data object; their contents follow.
    for (const Item& item : m.items) {
        std::wstring path;
        if (!SafePath(drag->staging, item.name, path)) {
            Log("a bad name from the host: %s", item.name.c_str());
            DeleteTree(drag->staging);
            SendDragFinished(m.id, false);
            return;
        }
        if (item.dir) {
            CreateDirectoryW(Long(path).c_str(), nullptr);
        } else {
            HANDLE file = CreateFileW(Long(path).c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                                      FILE_ATTRIBUTE_NORMAL, nullptr);
            if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
        }
        drag->items.push_back(path);
    }
    SOCKET s = Connect(m.id);
    if (s == INVALID_SOCKET) {
        DeleteTree(drag->staging);
        SendDragFinished(m.id, false);
        return;
    }
    drag->socket = s;
    auto* job = new ReceiveJob{m.id, drag->staging, s};
    HANDLE thread = CreateThread(nullptr, 0, Receive, job, 0, nullptr);
    if (thread == nullptr) {
        delete job;
        closesocket(s);
        DeleteTree(drag->staging);
        SendDragFinished(m.id, false);
        return;
    }
    CloseHandle(thread);
    drag->window = HelperWindow();
    if (drag->window == nullptr) {
        g_host_drag = std::move(drag);
        EndHostDrag(false);
        return;
    }
    SetTimer(drag->window, ARM_TIMER, ARM_TIMEOUT_MS, nullptr);
    SetTimer(drag->window, PRESS_TIMER, PRESS_POLL_MS, nullptr);
    g_cancelled_drag = 0;
    // Armed a moment later, once the window is surely in place for the press.
    SetTimer(drag->window, ARMED_TIMER, ARMED_DELAY_MS, nullptr);
    g_host_drag = std::move(drag);
}

void CancelHostDrag(uint32_t drag) {
    if (!g_host_drag || g_host_drag->drag != drag) return;
    if (g_host_drag->started) {
        // QueryContinueDrag ends it at the next pointer event, which the worker makes.
        g_cancelled_drag = drag;
    } else {
        EndHostDrag(false);
    }
}

// --- A drag out to the host ------------------------------------------------------------

struct GuestDrag {
    uint32_t probe = 0;
    std::vector<std::wstring> paths;  // what the guest's drag carries
    bool answered = false;
};

GuestDrag g_guest_drag;
HWND g_catcher = nullptr;
constexpr UINT_PTR PROBE_TIMER = 3;
// The worker nudges the pointer every 40 ms, so a drag reaches the catcher at once;
// meanwhile the catcher would take a click meant for what's under it.
constexpr UINT PROBE_TIMEOUT_MS = 700;
// How long the catcher stays up after it found a drag, for the worker's button release.
constexpr UINT HIDE_TIMEOUT_MS = 3000;

void SendNoGuestDrag(uint32_t probe) {
    Writer w;
    w.u8(NO_GUEST_DRAG);
    w.u32(probe);
    Send(w);
}

// The files a data object carries (CF_HDROP).
std::vector<std::wstring> DroppedFiles(IDataObject* object) {
    std::vector<std::wstring> paths;
    FORMATETC format = {CF_HDROP, nullptr, DVASPECT_CONTENT, -1, TYMED_HGLOBAL};
    STGMEDIUM medium = {};
    if (FAILED(object->GetData(&format, &medium))) return paths;
    auto drop = static_cast<HDROP>(medium.hGlobal);
    UINT count = DragQueryFileW(drop, 0xFFFFFFFF, nullptr, 0);
    for (UINT i = 0; i < count; i++) {
        UINT len = DragQueryFileW(drop, i, nullptr, 0);
        std::wstring path(len + 1, L'\0');
        DragQueryFileW(drop, i, path.data(), len + 1);
        path.resize(len);
        paths.push_back(path);
    }
    ReleaseStgMedium(&medium);
    return paths;
}

void HideCatcher() {
    if (g_catcher != nullptr) {
        KillTimer(g_catcher, PROBE_TIMER);
        ShowWindow(g_catcher, SW_HIDE);
    }
}

class DropTarget : public IDropTarget {
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** out) override {
        if (riid == IID_IUnknown || riid == IID_IDropTarget) {
            *out = static_cast<IDropTarget*>(this);
            AddRef();
            return S_OK;
        }
        *out = nullptr;
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++refs_; }
    ULONG STDMETHODCALLTYPE Release() override {
        ULONG left = --refs_;
        if (left == 0) delete this;
        return left;
    }
    HRESULT STDMETHODCALLTYPE DragEnter(IDataObject* object, DWORD, POINTL, DWORD* effect) override {
        has_files_ = false;
        if (g_guest_drag.probe != 0 && !g_guest_drag.answered) {
            g_guest_drag.answered = true;
            KillTimer(g_catcher, PROBE_TIMER);
            g_guest_drag.paths = DroppedFiles(object);
            if (g_guest_drag.paths.empty()) {
                Log("the guest's drag carries no files");
                SendNoGuestDrag(g_guest_drag.probe);
                HideCatcher();
            } else {
                Writer w;
                w.u8(GUEST_DRAG);
                w.u32(g_guest_drag.probe);
                std::vector<Item> items;
                for (const std::wstring& path : g_guest_drag.paths) {
                    DWORD attributes = GetFileAttributesW(path.c_str());
                    const wchar_t* slash = wcsrchr(path.c_str(), L'\\');
                    Item item;
                    item.name = ToUtf8(slash != nullptr ? slash + 1 : path.c_str());
                    item.dir = attributes != INVALID_FILE_ATTRIBUTES && (attributes & FILE_ATTRIBUTE_DIRECTORY);
                    items.push_back(item);
                }
                w.items(items);
                Send(w);
                Log("the guest's drag carries %zu items", items.size());
                SetTimer(g_catcher, PROBE_TIMER, HIDE_TIMEOUT_MS, nullptr);
                has_files_ = true;
            }
        }
        *effect = has_files_ ? DROPEFFECT_COPY : DROPEFFECT_NONE;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE DragOver(DWORD, POINTL, DWORD* effect) override {
        *effect = has_files_ ? DROPEFFECT_COPY : DROPEFFECT_NONE;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE DragLeave() override { return S_OK; }
    HRESULT STDMETHODCALLTYPE Drop(IDataObject*, DWORD, POINTL, DWORD* effect) override {
        // Refused: the files go to the host, and stay where they are here.
        *effect = DROPEFFECT_NONE;
        has_files_ = false;
        HideCatcher();
        return S_OK;
    }

private:
    std::atomic<ULONG> refs_{1};
    bool has_files_ = false;
};

LRESULT CALLBACK CatcherProc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    switch (message) {
        case WM_MOUSEACTIVATE:
            return MA_NOACTIVATE;
        case WM_LBUTTONDOWN:
        case WM_RBUTTONDOWN:
        case WM_MBUTTONDOWN:
            // A click, so no drag: out of the way.
            if (g_guest_drag.probe != 0 && !g_guest_drag.answered) {
                g_guest_drag.answered = true;
                SendNoGuestDrag(g_guest_drag.probe);
            }
            HideCatcher();
            return 0;
        case WM_TIMER:
            if (wparam == PROBE_TIMER) {
                if (g_guest_drag.probe != 0 && !g_guest_drag.answered) {
                    g_guest_drag.answered = true;
                    SendNoGuestDrag(g_guest_drag.probe);
                }
                HideCatcher();
            }
            return 0;
        default:
            return DefWindowProcW(window, message, wparam, lparam);
    }
}

// Shows the catcher, an invisible window over the whole screen that takes drops.
void Probe(uint32_t probe) {
    g_guest_drag = GuestDrag{};
    g_guest_drag.probe = probe;
    int x = GetSystemMetrics(SM_XVIRTUALSCREEN), y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    int width = GetSystemMetrics(SM_CXVIRTUALSCREEN), height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    if (g_catcher == nullptr) {
        g_catcher = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                                    L"vmmcagent-catcher", L"vmmc drag", WS_POPUP, x, y, width, height, nullptr,
                                    nullptr, GetModuleHandleW(nullptr), nullptr);
        if (g_catcher == nullptr) {
            SendNoGuestDrag(probe);
            return;
        }
        SetLayeredWindowAttributes(g_catcher, 0, 1, LWA_ALPHA);
        auto* target = new DropTarget();
        RegisterDragDrop(g_catcher, target);
        target->Release();
    }
    SetWindowPos(g_catcher, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    // Painted now: until a layered window has painted, the pointer passes through it.
    RedrawWindow(g_catcher, nullptr, nullptr, RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW);
    SetTimer(g_catcher, PROBE_TIMER, PROBE_TIMEOUT_MS, nullptr);
}

struct SendJob {
    uint32_t transfer;
    std::vector<std::wstring> paths;
};

std::atomic<uint32_t> g_cancelled_transfer{0};

struct Entry {
    std::wstring path;      // on this disk
    std::string relative;  // in the transfer
    bool dir;
    uint64_t size;
};

// `path` and everything in it (folders before their contents; links to folders aren't
// followed).
void Walk(const std::wstring& path, const std::string& relative, std::vector<Entry>& entries) {
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExW(Long(path).c_str(), GetFileExInfoStandard, &data)) return;
    bool dir = (data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
    uint64_t size = (static_cast<uint64_t>(data.nFileSizeHigh) << 32) | data.nFileSizeLow;
    entries.push_back(Entry{path, relative, dir, dir ? 0 : size});
    if (!dir || (data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) return;
    WIN32_FIND_DATAW found;
    HANDLE find = FindFirstFileW(Long(path + L"\\*").c_str(), &found);
    if (find == INVALID_HANDLE_VALUE) return;
    do {
        std::wstring name = found.cFileName;
        if (name == L"." || name == L"..") continue;
        Walk(path + L"\\" + name, relative + "/" + ToUtf8(name), entries);
    } while (FindNextFileW(find, &found));
    FindClose(find);
}

// Sends the guest's drag's files to the host.
DWORD WINAPI SendFiles(void* param) {
    std::unique_ptr<SendJob> job(static_cast<SendJob*>(param));
    SOCKET s = Connect(job->transfer);
    if (s == INVALID_SOCKET) return 0;
    std::vector<Entry> entries;
    for (const std::wstring& path : job->paths) {
        const wchar_t* slash = wcsrchr(path.c_str(), L'\\');
        Walk(path, ToUtf8(slash != nullptr ? slash + 1 : path.c_str()), entries);
    }
    uint64_t files = 0, bytes = 0;
    for (const Entry& entry : entries) {
        if (!entry.dir) {
            files++;
            bytes += entry.size;
        }
    }
    Writer total = ChunkMessage(job->transfer, TOTAL);
    total.u64(files);
    total.u64(bytes);
    std::string error;
    bool ok = SendAll(s, total.frame());
    std::vector<char> buffer(MAX_DATA);
    for (const Entry& entry : entries) {
        if (!ok || g_cancelled_transfer.load() == job->transfer) break;
        HANDLE file = INVALID_HANDLE_VALUE;
        if (!entry.dir) {
            file = CreateFileW(Long(entry.path).c_str(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                               OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
            if (file == INVALID_HANDLE_VALUE) {
                Log("can't read %s: skipped", entry.relative.c_str());
                continue;
            }
        }
        Writer begin = ChunkMessage(job->transfer, BEGIN);
        begin.str(entry.relative);
        begin.u8(entry.dir ? 1 : 0);
        begin.u64(entry.size);
        ok = SendAll(s, begin.frame());
        if (file != INVALID_HANDLE_VALUE) {
            uint64_t left = entry.size;
            while (ok && left > 0) {
                DWORD want = static_cast<DWORD>(left < buffer.size() ? left : buffer.size());
                DWORD read = 0;
                if (!ReadFile(file, buffer.data(), want, &read, nullptr) || read == 0) break;
                Writer data = ChunkMessage(job->transfer, DATA);
                data.bytes(buffer.data(), read);
                ok = SendAll(s, data.frame());
                left -= read;
            }
            CloseHandle(file);
            if (ok) ok = SendAll(s, ChunkMessage(job->transfer, END).frame());
        }
    }
    if (g_cancelled_transfer.load() == job->transfer) {
        Log("the host called the transfer off");
    } else if (ok) {
        Writer finish = ChunkMessage(job->transfer, FINISH);
        finish.optional_str(nullptr);
        ok = SendAll(s, finish.frame());
        Log("sent %llu files (%llu bytes) to the host", files, bytes);
    } else {
        Log("sending files to the host failed");
    }
    shutdown(s, SD_SEND);
    char drain[256];
    while (recv(s, drain, sizeof(drain), 0) > 0) {
    }
    closesocket(s);
    return 0;
}

void StartSending(uint32_t probe, uint32_t transfer) {
    if (probe != g_guest_drag.probe || g_guest_drag.paths.empty()) {
        Log("asked for the files of a drag the agent doesn't know");
        return;
    }
    auto* job = new SendJob{transfer, g_guest_drag.paths};
    HANDLE thread = CreateThread(nullptr, 0, SendFiles, job, 0, nullptr);
    if (thread == nullptr) {
        delete job;
        return;
    }
    CloseHandle(thread);
}

// --- The window ------------------------------------------------------------------------

void Act(const Message& m) {
    switch (m.kind) {
        case CONFIG:
            EnterCriticalSection(&g_config_lock);
            g_data_address = {};
            g_data_address.sin_family = AF_INET;
            memcpy(&g_data_address.sin_addr, m.address, 4);
            g_data_address.sin_port = htons(m.port);
            memcpy(g_token, m.token, sizeof(g_token));
            g_has_config = true;
            LeaveCriticalSection(&g_config_lock);
            break;
        case HOST_DRAG:
            StartHostDrag(m);
            break;
        case CANCEL_DRAG:
            CancelHostDrag(m.id);
            break;
        case PROBE:
            Probe(m.id);
            break;
        case SEND_FILES:
            StartSending(m.id, m.second);
            break;
        case CANCEL_TRANSFER:
            g_cancelled_transfer = m.id;
            break;
        default:
            break;
    }
}

LRESULT CALLBACK WindowProc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    switch (message) {
        case WM_CLIPBOARDUPDATE:
            if (GetClipboardSequenceNumber() != g_own_sequence) SetTimer(window, READ_TIMER, READ_DELAY_MS, nullptr);
            return 0;
        case WM_TIMER:
            if (wparam == READ_TIMER) {
                KillTimer(window, READ_TIMER);
                if (GetClipboardSequenceNumber() != g_own_sequence) SendClipboard(window);
            }
            return 0;
        case WM_FROM_HOST: {
            std::unique_ptr<std::wstring> text(reinterpret_cast<std::wstring*>(lparam));
            SetClipboard(window, *text);
            return 0;
        }
        case WM_MESSAGE: {
            std::unique_ptr<Message> m(reinterpret_cast<Message*>(lparam));
            Act(*m);
            return 0;
        }
        default:
            return DefWindowProcW(window, message, wparam, lparam);
    }
}

void AddWindowClass(const wchar_t* name, WNDPROC proc, HINSTANCE instance) {
    WNDCLASSW window_class = {};
    window_class.lpfnWndProc = proc;
    window_class.hInstance = instance;
    window_class.lpszClassName = name;
    window_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
    // Something to paint: a layered window takes the pointer only once it has painted.
    window_class.hbrBackground = static_cast<HBRUSH>(GetStockObject(BLACK_BRUSH));
    RegisterClassW(&window_class);
}

}  // namespace

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR, int) {
    // One per session.
    HANDLE single = CreateMutexW(nullptr, TRUE, L"Local\\vmmcagent");
    if (single == nullptr || GetLastError() == ERROR_ALREADY_EXISTS) return 0;
    // Screen coordinates as they are, whatever the scaling.
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    if (FAILED(OleInitialize(nullptr))) return 1;
    WSADATA wsa;
    WSAStartup(MAKEWORD(2, 2), &wsa);
    InitializeCriticalSection(&g_outgoing_lock);
    InitializeCriticalSection(&g_config_lock);
    g_outgoing_ready = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    CleanStaging(true);

    AddWindowClass(L"vmmcagent", WindowProc, instance);
    AddWindowClass(L"vmmcagent-helper", HelperProc, instance);
    AddWindowClass(L"vmmcagent-catcher", CatcherProc, instance);
    g_window = CreateWindowExW(0, L"vmmcagent", L"vmmc agent", 0, 0, 0, 0, 0, HWND_MESSAGE, nullptr, instance,
                               nullptr);
    if (g_window == nullptr || !AddClipboardFormatListener(g_window)) return 1;
    CreateThread(nullptr, 0, PortReader, nullptr, 0, nullptr);
    CreateThread(nullptr, 0, PortWriter, nullptr, 0, nullptr);

    MSG message;
    while (GetMessageW(&message, nullptr, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    OleUninitialize();
    return 0;
}
