// vmmc display driver: a WDDM display-only driver (KMDOD) for vmmc's display adapter,
// which has the Bochs display register interface (PCI 1234:1111).
//
// - BAR 0 is the framebuffer, BAR 2 holds the monitor's EDID at 0x000 and the DISPI
//   mode registers at 0x500 (16 bits each).
// - Windows gets a list of resolutions (standard ones that fit the framebuffer, plus
//   the EDID's preferred one), and the driver sets the one it picks through DISPI.
// - Windows draws into system memory; PresentDisplayOnly copies the changed rectangles
//   into the framebuffer. There's no hardware cursor: Windows draws it.
//
// Written from Microsoft's documentation of the display-only driver interface.

extern "C" {
#include <ntddk.h>
#include <dispmprt.h>
#ifdef VMMC_TRACE
#include <ntstrsafe.h>
#endif
}

namespace {

#ifdef VMMC_TRACE
// Test builds trace to vmmc's debug console (I/O port 0x402, which `vmmc-vmx run
// --firmware-log` saves).
void DebugTrace(const char* format, ...) {
    char text[256];
    va_list args;
    va_start(args, format);
    if (NT_SUCCESS(RtlStringCbVPrintfA(text, sizeof(text), format, args))) {
        for (const char* c = text; *c != 0; c++) {
            WRITE_PORT_UCHAR(reinterpret_cast<PUCHAR>(0x402), static_cast<UCHAR>(*c));
        }
    }
    va_end(args);
}
#else
#define DebugTrace(...) ((void)0)
#endif

constexpr ULONG POOL_TAG = 'dmmV';

// The adapter's registers, in BAR 2.
constexpr ULONG EDID_OFFSET = 0x000;
constexpr ULONG EDID_SIZE = 128;
constexpr ULONG DISPI_OFFSET = 0x500;
constexpr USHORT DISPI_ID = 0;
constexpr USHORT DISPI_XRES = 1;
constexpr USHORT DISPI_YRES = 2;
constexpr USHORT DISPI_BPP = 3;
constexpr USHORT DISPI_ENABLE = 4;
constexpr USHORT DISPI_BANK = 5;
constexpr USHORT DISPI_VIRT_WIDTH = 6;
constexpr USHORT DISPI_VIRT_HEIGHT = 7;
constexpr USHORT DISPI_X_OFFSET = 8;
constexpr USHORT DISPI_Y_OFFSET = 9;
constexpr USHORT DISPI_ID_MIN = 0xb0c0;
constexpr USHORT DISPI_ENABLED = 0x01;
constexpr USHORT DISPI_LINEAR_FRAMEBUFFER = 0x40;
constexpr USHORT DISPI_KEEP_MEMORY = 0x80;
constexpr ULONG MAX_WIDTH = 7680;
constexpr ULONG MAX_HEIGHT = 4320;

constexpr ULONG BYTES_PER_PIXEL = 4;
constexpr D3DDDIFORMAT SOURCE_FORMAT = D3DDDIFMT_A8R8G8B8;
constexpr D3DDDIFORMAT FRAMEBUFFER_FORMAT = D3DDDIFMT_X8R8G8B8;

// One source (the desktop) shown on one target (the monitor).
constexpr D3DDDI_VIDEO_PRESENT_SOURCE_ID SOURCE_ID = 0;
constexpr D3DDDI_VIDEO_PRESENT_TARGET_ID TARGET_ID = 0;
constexpr ULONG CHILD_UID = 0;

struct Mode {
    ULONG Width;
    ULONG Height;
};

// The standard resolutions offered, where the framebuffer has room for them.
const Mode STANDARD_MODES[] = {
    {640, 480},   {800, 600},   {1024, 768},  {1152, 864},  {1280, 720},
    {1280, 800},  {1280, 1024}, {1366, 768},  {1440, 900},  {1600, 900},
    {1600, 1200}, {1680, 1050}, {1920, 1080}, {1920, 1200}, {2048, 1152},
    {2560, 1080}, {2560, 1440}, {2560, 1600}, {3440, 1440}, {3840, 2160},
};
constexpr ULONG MAX_MODES = ARRAYSIZE(STANDARD_MODES) + 2;

struct Adapter {
    DXGKRNL_INTERFACE Dxgk;
    bool Started;

    PHYSICAL_ADDRESS FramebufferAddress;
    SIZE_T FramebufferSize;
    PUCHAR Framebuffer;
    PHYSICAL_ADDRESS RegistersAddress;
    SIZE_T RegistersSize;
    PUCHAR Registers;

    UCHAR Edid[EDID_SIZE];
    bool EdidValid;

    Mode Modes[MAX_MODES];
    ULONG ModeCount;
    Mode Preferred;

    // What's on the screen now.
    Mode Current;
    ULONG CurrentPitch;
    bool Visible;
};

Adapter* AdapterFrom(const void* context) {
    return static_cast<Adapter*>(const_cast<void*>(context));
}

void WriteDispi(Adapter* adapter, USHORT index, USHORT value) {
    WRITE_REGISTER_USHORT(
        reinterpret_cast<PUSHORT>(adapter->Registers + DISPI_OFFSET + index * 2), value);
}

USHORT ReadDispi(Adapter* adapter, USHORT index) {
    return READ_REGISTER_USHORT(
        reinterpret_cast<PUSHORT>(adapter->Registers + DISPI_OFFSET + index * 2));
}

bool SameSize(const Mode& a, const Mode& b) {
    return a.Width == b.Width && a.Height == b.Height;
}

bool Fits(const Adapter* adapter, const Mode& mode) {
    return mode.Width >= 640 && mode.Height >= 480 && mode.Width <= MAX_WIDTH &&
           mode.Height <= MAX_HEIGHT &&
           static_cast<SIZE_T>(mode.Width) * mode.Height * BYTES_PER_PIXEL <=
               adapter->FramebufferSize;
}

bool Offered(const Adapter* adapter, const Mode& mode) {
    for (ULONG i = 0; i < adapter->ModeCount; i++) {
        if (SameSize(adapter->Modes[i], mode)) {
            return true;
        }
    }
    return false;
}

void Offer(Adapter* adapter, const Mode& mode) {
    if (!Fits(adapter, mode) || Offered(adapter, mode) || adapter->ModeCount == MAX_MODES) {
        return;
    }
    // Kept in order of size, smallest first.
    ULONG at = adapter->ModeCount;
    while (at > 0) {
        const Mode& before = adapter->Modes[at - 1];
        ULONGLONG area = static_cast<ULONGLONG>(before.Width) * before.Height;
        if (area < static_cast<ULONGLONG>(mode.Width) * mode.Height ||
            (area == static_cast<ULONGLONG>(mode.Width) * mode.Height &&
             before.Width < mode.Width)) {
            break;
        }
        adapter->Modes[at] = before;
        at--;
    }
    adapter->Modes[at] = mode;
    adapter->ModeCount++;
}

// The EDID's preferred mode: its first detailed timing.
bool EdidPreferred(const UCHAR* edid, Mode* mode) {
    const UCHAR header[] = {0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00};
    if (RtlCompareMemory(edid, header, sizeof(header)) != sizeof(header)) {
        return false;
    }
    UCHAR sum = 0;
    for (ULONG i = 0; i < EDID_SIZE; i++) {
        sum = static_cast<UCHAR>(sum + edid[i]);
    }
    if (sum != 0) {
        return false;
    }
    const UCHAR* timing = edid + 54;
    if (timing[0] == 0 && timing[1] == 0) {
        return false;
    }
    mode->Width = timing[2] | ((timing[4] & 0xf0) << 4);
    mode->Height = timing[5] | ((timing[7] & 0xf0) << 4);
    return true;
}

void SetMode(Adapter* adapter, const Mode& mode) {
    DebugTrace("vmmcdisp: set mode %lux%lu\n", mode.Width, mode.Height);
    WriteDispi(adapter, DISPI_ENABLE, 0);
    WriteDispi(adapter, DISPI_BPP, static_cast<USHORT>(BYTES_PER_PIXEL * 8));
    WriteDispi(adapter, DISPI_XRES, static_cast<USHORT>(mode.Width));
    WriteDispi(adapter, DISPI_YRES, static_cast<USHORT>(mode.Height));
    WriteDispi(adapter, DISPI_BANK, 0);
    WriteDispi(adapter, DISPI_VIRT_WIDTH, static_cast<USHORT>(mode.Width));
    WriteDispi(adapter, DISPI_VIRT_HEIGHT, static_cast<USHORT>(mode.Height));
    WriteDispi(adapter, DISPI_X_OFFSET, 0);
    WriteDispi(adapter, DISPI_Y_OFFSET, 0);
    // Black until Windows draws the new mode in full.
    RtlZeroMemory(adapter->Framebuffer,
                  static_cast<SIZE_T>(mode.Width) * mode.Height * BYTES_PER_PIXEL);
    WriteDispi(adapter, DISPI_ENABLE,
               DISPI_ENABLED | DISPI_LINEAR_FRAMEBUFFER | DISPI_KEEP_MEMORY);
    adapter->Current = mode;
    adapter->CurrentPitch = mode.Width * BYTES_PER_PIXEL;
}

// Copies `rect` of a `pitch`-wide picture at `source` to the same place on the screen.
void CopyToScreen(Adapter* adapter, const UCHAR* source, LONG pitch, RECT rect) {
    rect.left = max(rect.left, 0L);
    rect.top = max(rect.top, 0L);
    rect.right = min(rect.right, static_cast<LONG>(adapter->Current.Width));
    rect.bottom = min(rect.bottom, static_cast<LONG>(adapter->Current.Height));
    if (rect.left >= rect.right || rect.top >= rect.bottom) {
        return;
    }
    SIZE_T bytes = static_cast<SIZE_T>(rect.right - rect.left) * BYTES_PER_PIXEL;
    for (LONG y = rect.top; y < rect.bottom; y++) {
        RtlCopyMemory(adapter->Framebuffer + static_cast<SIZE_T>(y) * adapter->CurrentPitch +
                          static_cast<SIZE_T>(rect.left) * BYTES_PER_PIXEL,
                      source + static_cast<LONGLONG>(y) * pitch +
                          static_cast<LONGLONG>(rect.left) * BYTES_PER_PIXEL,
                      bytes);
    }
}

// A memory resource's length: large ones (above 4 GiB in size) count in bigger units.
ULONGLONG MemoryLength(const CM_PARTIAL_RESOURCE_DESCRIPTOR& resource) {
    if (resource.Type == CmResourceTypeMemory) {
        return resource.u.Memory.Length;
    }
    switch (resource.Flags & (CM_RESOURCE_MEMORY_LARGE_40 | CM_RESOURCE_MEMORY_LARGE_48 |
                              CM_RESOURCE_MEMORY_LARGE_64)) {
    case CM_RESOURCE_MEMORY_LARGE_40:
        return static_cast<ULONGLONG>(resource.u.Memory40.Length40) << 8;
    case CM_RESOURCE_MEMORY_LARGE_48:
        return static_cast<ULONGLONG>(resource.u.Memory48.Length48) << 16;
    case CM_RESOURCE_MEMORY_LARGE_64:
        return static_cast<ULONGLONG>(resource.u.Memory64.Length64) << 32;
    default:
        return 0;
    }
}

void UnmapAll(Adapter* adapter) {
    if (adapter->Framebuffer != nullptr) {
        MmUnmapIoSpace(adapter->Framebuffer, adapter->FramebufferSize);
        adapter->Framebuffer = nullptr;
    }
    if (adapter->Registers != nullptr) {
        MmUnmapIoSpace(adapter->Registers, adapter->RegistersSize);
        adapter->Registers = nullptr;
    }
}

// A mode's signal, as a display-only driver describes it: just its size. There's no
// real signal to time, and Windows refuses target modes whose frequencies a
// display-only driver specifies (STATUS_GRAPHICS_INVALID_FREQUENCY).
void FillSignalInfo(D3DKMDT_VIDEO_SIGNAL_INFO* signal, const Mode& mode) {
    signal->VideoStandard = D3DKMDT_VSS_OTHER;
    signal->TotalSize.cx = mode.Width;
    signal->TotalSize.cy = mode.Height;
    signal->ActiveSize = signal->TotalSize;
    signal->VSyncFreq.Numerator = D3DKMDT_FREQUENCY_NOTSPECIFIED;
    signal->VSyncFreq.Denominator = D3DKMDT_FREQUENCY_NOTSPECIFIED;
    signal->HSyncFreq.Numerator = D3DKMDT_FREQUENCY_NOTSPECIFIED;
    signal->HSyncFreq.Denominator = D3DKMDT_FREQUENCY_NOTSPECIFIED;
    signal->PixelRate = D3DKMDT_FREQUENCY_NOTSPECIFIED;
    signal->ScanLineOrdering = D3DDDI_VSSLO_PROGRESSIVE;
}

NTSTATUS AddSourceMode(D3DKMDT_HVIDPNSOURCEMODESET set,
                       const DXGK_VIDPNSOURCEMODESET_INTERFACE* modes, const Mode& mode) {
    D3DKMDT_VIDPN_SOURCE_MODE* info = nullptr;
    NTSTATUS status = modes->pfnCreateNewModeInfo(set, &info);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    info->Type = D3DKMDT_RMT_GRAPHICS;
    info->Format.Graphics.PrimSurfSize.cx = mode.Width;
    info->Format.Graphics.PrimSurfSize.cy = mode.Height;
    info->Format.Graphics.VisibleRegionSize = info->Format.Graphics.PrimSurfSize;
    info->Format.Graphics.Stride = mode.Width * BYTES_PER_PIXEL;
    info->Format.Graphics.PixelFormat = SOURCE_FORMAT;
    info->Format.Graphics.ColorBasis = D3DKMDT_CB_SRGB;
    info->Format.Graphics.PixelValueAccessMode = D3DKMDT_PVAM_DIRECT;
    status = modes->pfnAddMode(set, info);
    if (!NT_SUCCESS(status)) {
        DebugTrace("vmmcdisp: source mode %lux%lu refused: %lx\n", mode.Width, mode.Height, status);
        modes->pfnReleaseModeInfo(set, info);
    }
    return status;
}

NTSTATUS AddTargetMode(const Adapter* adapter, D3DKMDT_HVIDPNTARGETMODESET set,
                       const DXGK_VIDPNTARGETMODESET_INTERFACE* modes, const Mode& mode) {
    D3DKMDT_VIDPN_TARGET_MODE* info = nullptr;
    NTSTATUS status = modes->pfnCreateNewModeInfo(set, &info);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    FillSignalInfo(&info->VideoSignalInfo, mode);
    info->Preference =
        SameSize(mode, adapter->Preferred) ? D3DKMDT_MP_PREFERRED : D3DKMDT_MP_NOTPREFERRED;
    status = modes->pfnAddMode(set, info);
    if (!NT_SUCCESS(status)) {
        DebugTrace("vmmcdisp: target mode %lux%lu refused: %lx\n", mode.Width, mode.Height, status);
        modes->pfnReleaseModeInfo(set, info);
    }
    return status;
}

// Whether a pinned source mode is one this adapter can show.
bool SourceModeSupported(const Adapter* adapter, const D3DKMDT_VIDPN_SOURCE_MODE* info) {
    if (info->Type != D3DKMDT_RMT_GRAPHICS ||
        info->Format.Graphics.PixelFormat != SOURCE_FORMAT) {
        return false;
    }
    Mode mode = {info->Format.Graphics.PrimSurfSize.cx, info->Format.Graphics.PrimSurfSize.cy};
    return Offered(adapter, mode) &&
           SameSize(mode, {info->Format.Graphics.VisibleRegionSize.cx,
                           info->Format.Graphics.VisibleRegionSize.cy});
}

// ---- Device DDIs ----

NTSTATUS AddDevice(IN_CONST_PDEVICE_OBJECT, OUT_PPVOID context) {
    auto adapter =
        static_cast<Adapter*>(ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(Adapter), POOL_TAG));
    if (adapter == nullptr) {
        return STATUS_NO_MEMORY;
    }
    *context = adapter;
    return STATUS_SUCCESS;
}

NTSTATUS StartDevice(IN_CONST_PVOID context, IN_PDXGK_START_INFO, IN_PDXGKRNL_INTERFACE dxgk,
                     OUT_PULONG sources, OUT_PULONG children) {
    Adapter* adapter = AdapterFrom(context);
    adapter->Dxgk = *dxgk;

    DXGK_DEVICE_INFO device = {};
    NTSTATUS status = dxgk->DxgkCbGetDeviceInformation(dxgk->DeviceHandle, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    // The framebuffer is the large memory range, the registers the small one.
    const CM_RESOURCE_LIST* resources = device.TranslatedResourceList;
    for (ULONG list = 0; resources != nullptr && list < resources->Count; list++) {
        const CM_PARTIAL_RESOURCE_LIST& partial = resources->List[list].PartialResourceList;
        for (ULONG i = 0; i < partial.Count; i++) {
            const CM_PARTIAL_RESOURCE_DESCRIPTOR& resource = partial.PartialDescriptors[i];
            if (resource.Type != CmResourceTypeMemory &&
                resource.Type != CmResourceTypeMemoryLarge) {
                continue;
            }
            PHYSICAL_ADDRESS start = resource.u.Memory.Start;
            ULONGLONG length = MemoryLength(resource);
            if (length >= 0x100000 && length > adapter->FramebufferSize) {
                adapter->FramebufferAddress = start;
                adapter->FramebufferSize = static_cast<SIZE_T>(length);
            } else if (length >= DISPI_OFFSET + 0x20 && length < 0x100000) {
                adapter->RegistersAddress = start;
                adapter->RegistersSize = static_cast<SIZE_T>(length);
            }
        }
    }
    if (adapter->FramebufferSize == 0 || adapter->RegistersSize == 0) {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    adapter->Registers = static_cast<PUCHAR>(MmMapIoSpaceEx(
        adapter->RegistersAddress, adapter->RegistersSize, PAGE_READWRITE | PAGE_NOCACHE));
    adapter->Framebuffer = static_cast<PUCHAR>(MmMapIoSpaceEx(
        adapter->FramebufferAddress, adapter->FramebufferSize, PAGE_READWRITE | PAGE_WRITECOMBINE));
    if (adapter->Registers == nullptr || adapter->Framebuffer == nullptr) {
        UnmapAll(adapter);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    if (ReadDispi(adapter, DISPI_ID) < DISPI_ID_MIN) {
        UnmapAll(adapter);
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    READ_REGISTER_BUFFER_UCHAR(adapter->Registers + EDID_OFFSET, adapter->Edid, EDID_SIZE);
    Mode preferred = {};
    adapter->EdidValid = EdidPreferred(adapter->Edid, &preferred);

    // What the firmware left on the screen.
    DXGK_DISPLAY_INFORMATION post = {};
    status = dxgk->DxgkCbAcquirePostDisplayOwnership(dxgk->DeviceHandle, &post);
    Mode firmware = {};
    if (NT_SUCCESS(status) && post.Width != 0 && post.Height != 0) {
        firmware = {post.Width, post.Height};
    }

    adapter->ModeCount = 0;
    for (const Mode& mode : STANDARD_MODES) {
        Offer(adapter, mode);
    }
    if (adapter->EdidValid) {
        Offer(adapter, preferred);
    }
    Offer(adapter, firmware);
    if (adapter->ModeCount == 0) {
        UnmapAll(adapter);
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    if (adapter->EdidValid && Offered(adapter, preferred)) {
        adapter->Preferred = preferred;
    } else if (Offered(adapter, firmware)) {
        adapter->Preferred = firmware;
    } else {
        adapter->Preferred = adapter->Modes[0];
    }

    if (Offered(adapter, firmware) && post.Pitch != 0) {
        adapter->Current = firmware;
        adapter->CurrentPitch = post.Pitch;
    } else {
        SetMode(adapter, adapter->Preferred);
    }
    adapter->Visible = true;
    adapter->Started = true;
    DebugTrace("vmmcdisp: started: framebuffer %llx+%llx, registers %llx, EDID %d (%lux%lu), "
          "firmware %lux%lu pitch %lu, %lu modes, showing %lux%lu\n",
          adapter->FramebufferAddress.QuadPart, static_cast<ULONGLONG>(adapter->FramebufferSize),
          adapter->RegistersAddress.QuadPart, adapter->EdidValid ? 1 : 0, preferred.Width,
          preferred.Height, firmware.Width, firmware.Height, post.Pitch, adapter->ModeCount,
          adapter->Current.Width, adapter->Current.Height);
    *sources = 1;
    *children = 1;
    return STATUS_SUCCESS;
}

NTSTATUS StopDevice(IN_CONST_PVOID context) {
    Adapter* adapter = AdapterFrom(context);
    adapter->Started = false;
    UnmapAll(adapter);
    return STATUS_SUCCESS;
}

NTSTATUS RemoveDevice(IN_CONST_PVOID context) {
    Adapter* adapter = AdapterFrom(context);
    UnmapAll(adapter);
    ExFreePoolWithTag(adapter, POOL_TAG);
    return STATUS_SUCCESS;
}

NTSTATUS DispatchIoRequest(IN_CONST_PVOID, IN_ULONG, IN_PVIDEO_REQUEST_PACKET) {
    return STATUS_NOT_SUPPORTED;
}

BOOLEAN InterruptRoutine(IN_CONST_PVOID, IN_ULONG) {
    // The adapter has no interrupts.
    return FALSE;
}

VOID DpcRoutine(IN_CONST_PVOID) {}

NTSTATUS QueryChildRelations(IN_CONST_PVOID, PDXGK_CHILD_DESCRIPTOR relations, ULONG size) {
    // Room for the children and a terminating empty entry.
    if (size < 2 * sizeof(DXGK_CHILD_DESCRIPTOR)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    relations[0].ChildDeviceType = TypeVideoOutput;
    relations[0].ChildCapabilities.HpdAwareness = HpdAwarenessAlwaysConnected;
    relations[0].ChildCapabilities.Type.VideoOutput.InterfaceTechnology = D3DKMDT_VOT_HD15;
    relations[0].ChildCapabilities.Type.VideoOutput.MonitorOrientationAwareness = D3DKMDT_MOA_NONE;
    relations[0].ChildCapabilities.Type.VideoOutput.SupportsSdtvModes = FALSE;
    relations[0].AcpiUid = 0;
    relations[0].ChildUid = CHILD_UID;
    return STATUS_SUCCESS;
}

NTSTATUS QueryChildStatus(IN_CONST_PVOID, INOUT_PDXGK_CHILD_STATUS status, IN_BOOLEAN) {
    switch (status->Type) {
    case StatusConnection:
        status->HotPlug.Connected = TRUE;
        return STATUS_SUCCESS;
    case StatusRotation:
        status->Rotation.Angle = 0;
        return STATUS_SUCCESS;
    default:
        return STATUS_NOT_SUPPORTED;
    }
}

NTSTATUS QueryDeviceDescriptor(IN_CONST_PVOID context, IN_ULONG child,
                               INOUT_PDXGK_DEVICE_DESCRIPTOR descriptor) {
    Adapter* adapter = AdapterFrom(context);
    if (child != CHILD_UID) {
        return STATUS_INVALID_PARAMETER;
    }
    if (!adapter->EdidValid) {
        return STATUS_MONITOR_NO_DESCRIPTOR;
    }
    if (descriptor->DescriptorOffset >= EDID_SIZE) {
        return STATUS_MONITOR_NO_MORE_DESCRIPTOR_DATA;
    }
    ULONG length = min(descriptor->DescriptorLength, EDID_SIZE - descriptor->DescriptorOffset);
    RtlCopyMemory(descriptor->DescriptorBuffer, adapter->Edid + descriptor->DescriptorOffset,
                  length);
    descriptor->DescriptorLength = length;
    return STATUS_SUCCESS;
}

NTSTATUS SetPowerState(IN_CONST_PVOID context, IN_ULONG device, IN_DEVICE_POWER_STATE state,
                       IN_POWER_ACTION) {
    Adapter* adapter = AdapterFrom(context);
    // Back in D0, the adapter may have lost its mode (after hibernation, say).
    if (device == DISPLAY_ADAPTER_HW_ID && state == PowerDeviceD0 && adapter->Started &&
        (ReadDispi(adapter, DISPI_XRES) != adapter->Current.Width ||
         ReadDispi(adapter, DISPI_YRES) != adapter->Current.Height)) {
        SetMode(adapter, adapter->Current);
    }
    return STATUS_SUCCESS;
}

NTSTATUS NotifyAcpiEvent(IN_CONST_PVOID, IN_DXGK_EVENT_TYPE, IN_ULONG, IN_PVOID,
                         OUT_PULONG flags) {
    *flags = 0;
    return STATUS_SUCCESS;
}

VOID ResetDevice(IN_CONST_PVOID) {}

VOID Unload(VOID) {}

NTSTATUS QueryInterface(IN_CONST_PVOID, IN_PQUERY_INTERFACE) {
    return STATUS_NOT_SUPPORTED;
}

VOID ControlEtwLogging(IN_BOOLEAN, IN_ULONG, IN_UCHAR) {}

NTSTATUS StopDeviceAndReleasePostDisplayOwnership(PVOID context, D3DDDI_VIDEO_PRESENT_TARGET_ID,
                                                  PDXGK_DISPLAY_INFORMATION display) {
    Adapter* adapter = AdapterFrom(context);
    // Whoever takes over (Windows' basic display driver) carries on in this mode.
    display->Width = adapter->Current.Width;
    display->Height = adapter->Current.Height;
    display->Pitch = adapter->CurrentPitch;
    display->ColorFormat = FRAMEBUFFER_FORMAT;
    display->PhysicAddress = adapter->FramebufferAddress;
    display->TargetId = TARGET_ID;
    display->AcpiId = 0;
    return StopDevice(context);
}

// For the blue screen: the current mode, written to directly.
NTSTATUS SystemDisplayEnable(PVOID context, D3DDDI_VIDEO_PRESENT_TARGET_ID,
                             PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS, UINT* width, UINT* height,
                             D3DDDIFORMAT* format) {
    Adapter* adapter = AdapterFrom(context);
    *width = adapter->Current.Width;
    *height = adapter->Current.Height;
    *format = FRAMEBUFFER_FORMAT;
    return STATUS_SUCCESS;
}

VOID SystemDisplayWrite(PVOID context, PVOID source, UINT width, UINT height, UINT stride,
                        UINT x, UINT y) {
    Adapter* adapter = AdapterFrom(context);
    if (adapter->Framebuffer == nullptr) {
        return;
    }
    // The source lands at (x, y): shift it so CopyToScreen can index both alike.
    RECT rect = {static_cast<LONG>(x), static_cast<LONG>(y), static_cast<LONG>(x + width),
                 static_cast<LONG>(y + height)};
    const UCHAR* origin = static_cast<const UCHAR*>(source) -
                          static_cast<LONGLONG>(y) * stride -
                          static_cast<LONGLONG>(x) * BYTES_PER_PIXEL;
    CopyToScreen(adapter, origin, static_cast<LONG>(stride), rect);
}

// ---- Display DDIs ----

NTSTATUS APIENTRY QueryAdapterInfo(IN_CONST_HANDLE, IN_CONST_PDXGKARG_QUERYADAPTERINFO query) {
    if (query->Type != DXGKQAITYPE_DRIVERCAPS) {
        return STATUS_NOT_SUPPORTED;
    }
    if (query->OutputDataSize < sizeof(DXGK_DRIVERCAPS)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    auto caps = static_cast<DXGK_DRIVERCAPS*>(query->pOutputData);
    RtlZeroMemory(caps, query->OutputDataSize);
    caps->HighestAcceptableAddress.QuadPart = -1;
    caps->WDDMVersion = DXGKDDI_WDDMv1_2;
    caps->SupportNonVGA = TRUE;
    caps->SupportSmoothRotation = FALSE;
    return STATUS_SUCCESS;
}

NTSTATUS APIENTRY SetPointerPosition(IN_CONST_HANDLE, IN_CONST_PDXGKARG_SETPOINTERPOSITION) {
    return STATUS_SUCCESS;
}

NTSTATUS APIENTRY SetPointerShape(IN_CONST_HANDLE, IN_CONST_PDXGKARG_SETPOINTERSHAPE) {
    return STATUS_NOT_SUPPORTED;
}

// The VidPN's source mode set for our source, with its pinned mode if any. Release
// with Release().
struct PinnedSource {
    const DXGK_VIDPN_INTERFACE* VidPn = nullptr;
    D3DKMDT_HVIDPN Handle = nullptr;
    D3DKMDT_HVIDPNSOURCEMODESET Set = nullptr;
    const DXGK_VIDPNSOURCEMODESET_INTERFACE* Modes = nullptr;
    const D3DKMDT_VIDPN_SOURCE_MODE* Pinned = nullptr;

    NTSTATUS Acquire(const DXGK_VIDPN_INTERFACE* vidpn, D3DKMDT_HVIDPN handle,
                     D3DDDI_VIDEO_PRESENT_SOURCE_ID source) {
        VidPn = vidpn;
        Handle = handle;
        NTSTATUS status = vidpn->pfnAcquireSourceModeSet(handle, source, &Set, &Modes);
        if (!NT_SUCCESS(status)) {
            Set = nullptr;
            return status;
        }
        status = Modes->pfnAcquirePinnedModeInfo(Set, &Pinned);
        if (status == STATUS_GRAPHICS_MODE_NOT_PINNED) {
            Pinned = nullptr;
            status = STATUS_SUCCESS;
        }
        return status;
    }

    void Release() {
        if (Pinned != nullptr) {
            Modes->pfnReleaseModeInfo(Set, Pinned);
            Pinned = nullptr;
        }
        if (Set != nullptr) {
            VidPn->pfnReleaseSourceModeSet(Handle, Set);
            Set = nullptr;
        }
    }
};

struct PinnedTarget {
    const DXGK_VIDPN_INTERFACE* VidPn = nullptr;
    D3DKMDT_HVIDPN Handle = nullptr;
    D3DKMDT_HVIDPNTARGETMODESET Set = nullptr;
    const DXGK_VIDPNTARGETMODESET_INTERFACE* Modes = nullptr;
    const D3DKMDT_VIDPN_TARGET_MODE* Pinned = nullptr;

    NTSTATUS Acquire(const DXGK_VIDPN_INTERFACE* vidpn, D3DKMDT_HVIDPN handle,
                     D3DDDI_VIDEO_PRESENT_TARGET_ID target) {
        VidPn = vidpn;
        Handle = handle;
        NTSTATUS status = vidpn->pfnAcquireTargetModeSet(handle, target, &Set, &Modes);
        if (!NT_SUCCESS(status)) {
            Set = nullptr;
            return status;
        }
        status = Modes->pfnAcquirePinnedModeInfo(Set, &Pinned);
        if (status == STATUS_GRAPHICS_MODE_NOT_PINNED) {
            Pinned = nullptr;
            status = STATUS_SUCCESS;
        }
        return status;
    }

    void Release() {
        if (Pinned != nullptr) {
            Modes->pfnReleaseModeInfo(Set, Pinned);
            Pinned = nullptr;
        }
        if (Set != nullptr) {
            VidPn->pfnReleaseTargetModeSet(Handle, Set);
            Set = nullptr;
        }
    }
};

NTSTATUS APIENTRY IsSupportedVidPn(IN_CONST_HANDLE context, INOUT_PDXGKARG_ISSUPPORTEDVIDPN query) {
    Adapter* adapter = AdapterFrom(context);
    query->IsVidPnSupported = FALSE;
    if (query->hDesiredVidPn == nullptr) {
        query->IsVidPnSupported = TRUE;
        return STATUS_SUCCESS;
    }
    const DXGK_VIDPN_INTERFACE* vidpn = nullptr;
    NTSTATUS status = adapter->Dxgk.DxgkCbQueryVidPnInterface(
        query->hDesiredVidPn, DXGK_VIDPN_INTERFACE_VERSION_V1, &vidpn);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    D3DKMDT_HVIDPNTOPOLOGY topology = nullptr;
    const DXGK_VIDPNTOPOLOGY_INTERFACE* paths = nullptr;
    status = vidpn->pfnGetTopology(query->hDesiredVidPn, &topology, &paths);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    SIZE_T count = 0;
    status = paths->pfnGetNumPaths(topology, &count);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    if (count == 0) {
        query->IsVidPnSupported = TRUE;
        return STATUS_SUCCESS;
    }
    if (count > 1) {
        return STATUS_SUCCESS;
    }
    PinnedSource source;
    status = source.Acquire(vidpn, query->hDesiredVidPn, SOURCE_ID);
    if (NT_SUCCESS(status)) {
        query->IsVidPnSupported =
            source.Pinned == nullptr || SourceModeSupported(adapter, source.Pinned);
    }
    source.Release();
    return status;
}

NTSTATUS APIENTRY RecommendFunctionalVidPn(IN_CONST_HANDLE,
                                           IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST) {
    return STATUS_GRAPHICS_NO_RECOMMENDED_FUNCTIONAL_VIDPN;
}

// Fills the unpinned mode sets of one path with what goes with the other side's pinned
// mode (or everything), and says the path supports neither scaling nor rotation.
NTSTATUS EnumPathModality(Adapter* adapter, const DXGK_VIDPN_INTERFACE* vidpn,
                          D3DKMDT_HVIDPN handle, D3DKMDT_HVIDPNTOPOLOGY topology,
                          const DXGK_VIDPNTOPOLOGY_INTERFACE* paths,
                          const D3DKMDT_VIDPN_PRESENT_PATH* path,
                          const DXGKARG_ENUMVIDPNCOFUNCMODALITY* query) {
    bool sourcePivot = query->EnumPivotType == D3DKMDT_EPT_VIDPNSOURCE &&
                       query->EnumPivot.VidPnSourceId == path->VidPnSourceId;
    bool targetPivot = query->EnumPivotType == D3DKMDT_EPT_VIDPNTARGET &&
                       query->EnumPivot.VidPnTargetId == path->VidPnTargetId;
    bool scalingPivot = query->EnumPivotType == D3DKMDT_EPT_SCALING &&
                        query->EnumPivot.VidPnSourceId == path->VidPnSourceId &&
                        query->EnumPivot.VidPnTargetId == path->VidPnTargetId;
    bool rotationPivot = query->EnumPivotType == D3DKMDT_EPT_ROTATION &&
                         query->EnumPivot.VidPnSourceId == path->VidPnSourceId &&
                         query->EnumPivot.VidPnTargetId == path->VidPnTargetId;

    PinnedSource source;
    PinnedTarget target;
    NTSTATUS status = source.Acquire(vidpn, handle, path->VidPnSourceId);
    if (NT_SUCCESS(status)) {
        status = target.Acquire(vidpn, handle, path->VidPnTargetId);
    }
    // What the pinned side asks for, if a side is pinned.
    Mode pinnedSource = {};
    Mode pinnedTarget = {};
    bool hasSource = NT_SUCCESS(status) && source.Pinned != nullptr;
    bool hasTarget = NT_SUCCESS(status) && target.Pinned != nullptr;
    if (hasSource) {
        pinnedSource = {source.Pinned->Format.Graphics.PrimSurfSize.cx,
                        source.Pinned->Format.Graphics.PrimSurfSize.cy};
    }
    if (hasTarget) {
        pinnedTarget = {target.Pinned->VideoSignalInfo.ActiveSize.cx,
                        target.Pinned->VideoSignalInfo.ActiveSize.cy};
    }
    source.Release();
    target.Release();
    if (!NT_SUCCESS(status)) {
        return status;
    }

    if (!sourcePivot && !hasSource) {
        D3DKMDT_HVIDPNSOURCEMODESET set = nullptr;
        const DXGK_VIDPNSOURCEMODESET_INTERFACE* modes = nullptr;
        status = vidpn->pfnCreateNewSourceModeSet(handle, path->VidPnSourceId, &set, &modes);
        if (!NT_SUCCESS(status)) {
            return status;
        }
        // Windows may turn a mode down; the others still count.
        ULONG added = 0;
        for (ULONG i = 0; i < adapter->ModeCount; i++) {
            if ((!hasTarget || SameSize(adapter->Modes[i], pinnedTarget)) &&
                NT_SUCCESS(AddSourceMode(set, modes, adapter->Modes[i]))) {
                added++;
            }
        }
        status = added > 0 ? STATUS_SUCCESS : STATUS_GRAPHICS_INVALID_VIDPN_SOURCEMODESET;
        if (NT_SUCCESS(status)) {
            status = vidpn->pfnAssignSourceModeSet(handle, path->VidPnSourceId, set);
        }
        if (!NT_SUCCESS(status)) {
            vidpn->pfnReleaseSourceModeSet(handle, set);
            return status;
        }
    }
    if (!targetPivot && !hasTarget) {
        D3DKMDT_HVIDPNTARGETMODESET set = nullptr;
        const DXGK_VIDPNTARGETMODESET_INTERFACE* modes = nullptr;
        status = vidpn->pfnCreateNewTargetModeSet(handle, path->VidPnTargetId, &set, &modes);
        if (!NT_SUCCESS(status)) {
            return status;
        }
        ULONG added = 0;
        for (ULONG i = 0; i < adapter->ModeCount; i++) {
            if ((!hasSource || SameSize(adapter->Modes[i], pinnedSource)) &&
                NT_SUCCESS(AddTargetMode(adapter, set, modes, adapter->Modes[i]))) {
                added++;
            }
        }
        status = added > 0 ? STATUS_SUCCESS : STATUS_GRAPHICS_INVALID_VIDPN_TARGETMODESET;
        if (NT_SUCCESS(status)) {
            status = vidpn->pfnAssignTargetModeSet(handle, path->VidPnTargetId, set);
        }
        if (!NT_SUCCESS(status)) {
            vidpn->pfnReleaseTargetModeSet(handle, set);
            return status;
        }
    }

    // The picture goes to the screen as it is: no scaling, no rotation.
    D3DKMDT_VIDPN_PRESENT_PATH update = *path;
    bool changed = false;
    if (!scalingPivot && update.ContentTransformation.Scaling == D3DKMDT_VPPS_UNPINNED) {
        RtlZeroMemory(&update.ContentTransformation.ScalingSupport,
                      sizeof(update.ContentTransformation.ScalingSupport));
        update.ContentTransformation.ScalingSupport.Identity = 1;
        changed = true;
    }
    if (!rotationPivot && update.ContentTransformation.Rotation == D3DKMDT_VPPR_UNPINNED) {
        RtlZeroMemory(&update.ContentTransformation.RotationSupport,
                      sizeof(update.ContentTransformation.RotationSupport));
        update.ContentTransformation.RotationSupport.Identity = 1;
        changed = true;
    }
    if (changed) {
        status = paths->pfnUpdatePathSupportInfo(topology, &update);
    }
    return status;
}

NTSTATUS APIENTRY EnumVidPnCofuncModality(IN_CONST_HANDLE context,
                                          IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST query) {
    Adapter* adapter = AdapterFrom(context);
    const DXGK_VIDPN_INTERFACE* vidpn = nullptr;
    NTSTATUS status = adapter->Dxgk.DxgkCbQueryVidPnInterface(
        query->hConstrainingVidPn, DXGK_VIDPN_INTERFACE_VERSION_V1, &vidpn);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    D3DKMDT_HVIDPNTOPOLOGY topology = nullptr;
    const DXGK_VIDPNTOPOLOGY_INTERFACE* paths = nullptr;
    status = vidpn->pfnGetTopology(query->hConstrainingVidPn, &topology, &paths);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    const D3DKMDT_VIDPN_PRESENT_PATH* path = nullptr;
    status = paths->pfnAcquireFirstPathInfo(topology, &path);
    if (status == STATUS_GRAPHICS_DATASET_IS_EMPTY) {
        return STATUS_SUCCESS;
    }
    while (NT_SUCCESS(status) && status != STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET) {
        status = EnumPathModality(adapter, vidpn, query->hConstrainingVidPn, topology, paths,
                                  path, query);
        const D3DKMDT_VIDPN_PRESENT_PATH* next = nullptr;
        if (NT_SUCCESS(status)) {
            status = paths->pfnAcquireNextPathInfo(topology, path, &next);
        }
        paths->pfnReleasePathInfo(topology, path);
        path = next;
    }
    if (path != nullptr) {
        paths->pfnReleasePathInfo(topology, path);
    }
    return status == STATUS_GRAPHICS_NO_MORE_ELEMENTS_IN_DATASET ? STATUS_SUCCESS : status;
}

NTSTATUS APIENTRY SetVidPnSourceVisibility(IN_CONST_HANDLE context,
                                           IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY visibility) {
    Adapter* adapter = AdapterFrom(context);
    if (visibility->VidPnSourceId != SOURCE_ID && visibility->VidPnSourceId != D3DDDI_ID_ALL) {
        return STATUS_SUCCESS;
    }
    adapter->Visible = visibility->Visible != FALSE;
    DebugTrace("vmmcdisp: visible %d\n", adapter->Visible ? 1 : 0);
    if (!adapter->Visible && adapter->Framebuffer != nullptr) {
        // As a monitor that's switched off: Windows presents everything again later.
        RtlZeroMemory(adapter->Framebuffer, static_cast<SIZE_T>(adapter->CurrentPitch) *
                                                adapter->Current.Height);
    }
    return STATUS_SUCCESS;
}

NTSTATUS APIENTRY CommitVidPn(IN_CONST_HANDLE context, IN_CONST_PDXGKARG_COMMITVIDPN_CONST commit) {
    Adapter* adapter = AdapterFrom(context);
    if (commit->AffectedVidPnSourceId != SOURCE_ID &&
        commit->AffectedVidPnSourceId != D3DDDI_ID_ALL) {
        return STATUS_SUCCESS;
    }
    const DXGK_VIDPN_INTERFACE* vidpn = nullptr;
    NTSTATUS status = adapter->Dxgk.DxgkCbQueryVidPnInterface(
        commit->hFunctionalVidPn, DXGK_VIDPN_INTERFACE_VERSION_V1, &vidpn);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    D3DKMDT_HVIDPNTOPOLOGY topology = nullptr;
    const DXGK_VIDPNTOPOLOGY_INTERFACE* paths = nullptr;
    status = vidpn->pfnGetTopology(commit->hFunctionalVidPn, &topology, &paths);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    SIZE_T count = 0;
    status = paths->pfnGetNumPaths(topology, &count);
    if (!NT_SUCCESS(status) || count == 0) {
        // The source shows nowhere: keep the mode, nothing is presented.
        return status;
    }
    PinnedSource source;
    status = source.Acquire(vidpn, commit->hFunctionalVidPn, SOURCE_ID);
    DebugTrace("vmmcdisp: commit source %lu, %llu paths, pinned %lux%lu, status %lx\n",
          commit->AffectedVidPnSourceId, static_cast<ULONGLONG>(count),
          source.Pinned != nullptr ? source.Pinned->Format.Graphics.PrimSurfSize.cx : 0,
          source.Pinned != nullptr ? source.Pinned->Format.Graphics.PrimSurfSize.cy : 0, status);
    if (NT_SUCCESS(status) && source.Pinned != nullptr) {
        if (SourceModeSupported(adapter, source.Pinned)) {
            Mode mode = {source.Pinned->Format.Graphics.PrimSurfSize.cx,
                         source.Pinned->Format.Graphics.PrimSurfSize.cy};
            if (!SameSize(mode, adapter->Current) ||
                adapter->CurrentPitch != mode.Width * BYTES_PER_PIXEL) {
                SetMode(adapter, mode);
            }
        } else {
            status = STATUS_GRAPHICS_INVALID_VIDPN;
        }
    }
    source.Release();
    return status;
}

NTSTATUS APIENTRY UpdateActiveVidPnPresentPath(
    IN_CONST_HANDLE, IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST update) {
    const D3DKMDT_VIDPN_PRESENT_PATH_TRANSFORMATION& transform =
        update->VidPnPresentPathInfo.ContentTransformation;
    if ((transform.Scaling != D3DKMDT_VPPS_IDENTITY && transform.Scaling != D3DKMDT_VPPS_UNPINNED &&
         transform.Scaling != D3DKMDT_VPPS_NOTSPECIFIED) ||
        (transform.Rotation != D3DKMDT_VPPR_IDENTITY && transform.Rotation != D3DKMDT_VPPR_UNPINNED &&
         transform.Rotation != D3DKMDT_VPPR_NOTSPECIFIED)) {
        return STATUS_INVALID_PARAMETER;
    }
    return STATUS_SUCCESS;
}

// Every offered mode is one the monitor takes, whatever its EDID lists.
NTSTATUS APIENTRY RecommendMonitorModes(IN_CONST_HANDLE context,
                                        IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST query) {
    Adapter* adapter = AdapterFrom(context);
    const DXGK_MONITORSOURCEMODESET_INTERFACE* modes = query->pMonitorSourceModeSetInterface;
    for (ULONG i = 0; i < adapter->ModeCount; i++) {
        D3DKMDT_MONITOR_SOURCE_MODE* info = nullptr;
        NTSTATUS status = modes->pfnCreateNewModeInfo(query->hMonitorSourceModeSet, &info);
        if (!NT_SUCCESS(status)) {
            return status;
        }
        FillSignalInfo(&info->VideoSignalInfo, adapter->Modes[i]);
        info->ColorBasis = D3DKMDT_CB_SRGB;
        info->ColorCoeffDynamicRanges.FirstChannel = 8;
        info->ColorCoeffDynamicRanges.SecondChannel = 8;
        info->ColorCoeffDynamicRanges.ThirdChannel = 8;
        info->ColorCoeffDynamicRanges.FourthChannel = 0;
        info->Origin = D3DKMDT_MCO_DRIVER;
        info->Preference = SameSize(adapter->Modes[i], adapter->Preferred)
                               ? D3DKMDT_MP_PREFERRED
                               : D3DKMDT_MP_NOTPREFERRED;
        status = modes->pfnAddMode(query->hMonitorSourceModeSet, info);
        if (!NT_SUCCESS(status)) {
            // The EDID listed it already, or Windows turned it down: the others count.
            DebugTrace("vmmcdisp: monitor mode %lux%lu refused: %lx\n", adapter->Modes[i].Width,
                       adapter->Modes[i].Height, status);
            modes->pfnReleaseModeInfo(query->hMonitorSourceModeSet, info);
        }
    }
    return STATUS_SUCCESS;
}

NTSTATUS APIENTRY QueryVidPnHWCapability(IN_CONST_HANDLE,
                                         INOUT_PDXGKARG_QUERYVIDPNHWCAPABILITY query) {
    RtlZeroMemory(&query->VidPnHWCaps, sizeof(query->VidPnHWCaps));
    return STATUS_SUCCESS;
}

NTSTATUS APIENTRY PresentDisplayOnly(IN_CONST_HANDLE context,
                                     IN_CONST_PDXGKARG_PRESENT_DISPLAYONLY present) {
    Adapter* adapter = AdapterFrom(context);
    static ULONG presents = 0;
    if (presents++ % 500 == 0) {
        DebugTrace("vmmcdisp: present %lu: %lu moves, %lu rects, pitch %ld, %lu bytes per pixel, "
              "visible %d\n",
              presents, present->NumMoves, present->NumDirtyRects, present->Pitch,
              present->BytesPerPixel, adapter->Visible ? 1 : 0);
    }
    if (present->VidPnSourceId != SOURCE_ID || !adapter->Visible ||
        adapter->Framebuffer == nullptr || present->BytesPerPixel != BYTES_PER_PIXEL) {
        return STATUS_SUCCESS;
    }
    // The source already holds the finished picture: a moved rectangle is copied from
    // where it ended up, like a changed one.
    auto source = static_cast<const UCHAR*>(present->pSource);
    for (ULONG i = 0; i < present->NumMoves; i++) {
        CopyToScreen(adapter, source, present->Pitch, present->pMoves[i].DestRect);
    }
    for (ULONG i = 0; i < present->NumDirtyRects; i++) {
        CopyToScreen(adapter, source, present->Pitch, present->pDirtyRect[i]);
    }
    return STATUS_SUCCESS;
}

} // namespace

extern "C" DRIVER_INITIALIZE DriverEntry;

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driver, PUNICODE_STRING registry) {
    KMDDOD_INITIALIZATION_DATA init = {};
    init.Version = DXGKDDI_INTERFACE_VERSION;
    init.DxgkDdiAddDevice = AddDevice;
    init.DxgkDdiStartDevice = StartDevice;
    init.DxgkDdiStopDevice = StopDevice;
    init.DxgkDdiRemoveDevice = RemoveDevice;
    init.DxgkDdiDispatchIoRequest = DispatchIoRequest;
    init.DxgkDdiInterruptRoutine = InterruptRoutine;
    init.DxgkDdiDpcRoutine = DpcRoutine;
    init.DxgkDdiQueryChildRelations = QueryChildRelations;
    init.DxgkDdiQueryChildStatus = QueryChildStatus;
    init.DxgkDdiQueryDeviceDescriptor = QueryDeviceDescriptor;
    init.DxgkDdiSetPowerState = SetPowerState;
    init.DxgkDdiNotifyAcpiEvent = NotifyAcpiEvent;
    init.DxgkDdiResetDevice = ResetDevice;
    init.DxgkDdiUnload = Unload;
    init.DxgkDdiQueryInterface = QueryInterface;
    init.DxgkDdiControlEtwLogging = ControlEtwLogging;
    init.DxgkDdiQueryAdapterInfo = QueryAdapterInfo;
    init.DxgkDdiSetPointerPosition = SetPointerPosition;
    init.DxgkDdiSetPointerShape = SetPointerShape;
    init.DxgkDdiIsSupportedVidPn = IsSupportedVidPn;
    init.DxgkDdiRecommendFunctionalVidPn = RecommendFunctionalVidPn;
    init.DxgkDdiEnumVidPnCofuncModality = EnumVidPnCofuncModality;
    init.DxgkDdiSetVidPnSourceVisibility = SetVidPnSourceVisibility;
    init.DxgkDdiCommitVidPn = CommitVidPn;
    init.DxgkDdiUpdateActiveVidPnPresentPath = UpdateActiveVidPnPresentPath;
    init.DxgkDdiRecommendMonitorModes = RecommendMonitorModes;
    init.DxgkDdiQueryVidPnHWCapability = QueryVidPnHWCapability;
    init.DxgkDdiPresentDisplayOnly = PresentDisplayOnly;
    init.DxgkDdiStopDeviceAndReleasePostDisplayOwnership = StopDeviceAndReleasePostDisplayOwnership;
    init.DxgkDdiSystemDisplayEnable = SystemDisplayEnable;
    init.DxgkDdiSystemDisplayWrite = SystemDisplayWrite;
#ifdef VMMC_TRACE
    // What Windows asks of the driver, and the answers.
    init.DxgkDdiQueryChildRelations = [](IN_CONST_PVOID context, PDXGK_CHILD_DESCRIPTOR relations,
                                         ULONG size) -> NTSTATUS {
        NTSTATUS status = QueryChildRelations(context, relations, size);
        DebugTrace("vmmcdisp: QueryChildRelations(%lu bytes) = %lx\n", size, status);
        return status;
    };
    init.DxgkDdiQueryChildStatus = [](IN_CONST_PVOID context, INOUT_PDXGK_CHILD_STATUS child,
                                      IN_BOOLEAN nonDestructive) -> NTSTATUS {
        NTSTATUS status = QueryChildStatus(context, child, nonDestructive);
        DebugTrace("vmmcdisp: QueryChildStatus(type %d, child %lu) = %lx\n", child->Type,
              child->ChildUid, status);
        return status;
    };
    init.DxgkDdiQueryDeviceDescriptor = [](IN_CONST_PVOID context, IN_ULONG child,
                                           INOUT_PDXGK_DEVICE_DESCRIPTOR descriptor) -> NTSTATUS {
        ULONG offset = descriptor->DescriptorOffset;
        ULONG length = descriptor->DescriptorLength;
        NTSTATUS status = QueryDeviceDescriptor(context, child, descriptor);
        DebugTrace("vmmcdisp: QueryDeviceDescriptor(child %lu, offset %lu, length %lu) = %lx\n", child,
              offset, length, status);
        return status;
    };
    init.DxgkDdiQueryAdapterInfo = [](IN_CONST_HANDLE context,
                                      IN_CONST_PDXGKARG_QUERYADAPTERINFO query) -> NTSTATUS {
        NTSTATUS status = QueryAdapterInfo(context, query);
        DebugTrace("vmmcdisp: QueryAdapterInfo(type %d, %u bytes) = %lx\n", query->Type,
              query->OutputDataSize, status);
        return status;
    };
    init.DxgkDdiIsSupportedVidPn = [](IN_CONST_HANDLE context,
                                      INOUT_PDXGKARG_ISSUPPORTEDVIDPN query) -> NTSTATUS {
        NTSTATUS status = IsSupportedVidPn(context, query);
        DebugTrace("vmmcdisp: IsSupportedVidPn = %lx, supported %d\n", status,
              query->IsVidPnSupported);
        return status;
    };
    init.DxgkDdiRecommendFunctionalVidPn =
        [](IN_CONST_HANDLE context,
           IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST query) -> NTSTATUS {
        NTSTATUS status = RecommendFunctionalVidPn(context, query);
        DebugTrace("vmmcdisp: RecommendFunctionalVidPn = %lx\n", status);
        return status;
    };
    init.DxgkDdiEnumVidPnCofuncModality =
        [](IN_CONST_HANDLE context,
           IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST query) -> NTSTATUS {
        NTSTATUS status = EnumVidPnCofuncModality(context, query);
        DebugTrace("vmmcdisp: EnumVidPnCofuncModality(pivot %d, source %lu, target %lu) = %lx\n",
              query->EnumPivotType, query->EnumPivot.VidPnSourceId,
              query->EnumPivot.VidPnTargetId, status);
        return status;
    };
    init.DxgkDdiCommitVidPn = [](IN_CONST_HANDLE context,
                                 IN_CONST_PDXGKARG_COMMITVIDPN_CONST commit) -> NTSTATUS {
        NTSTATUS status = CommitVidPn(context, commit);
        DebugTrace("vmmcdisp: CommitVidPn(source %lu) = %lx\n", commit->AffectedVidPnSourceId, status);
        return status;
    };
    init.DxgkDdiUpdateActiveVidPnPresentPath =
        [](IN_CONST_HANDLE context,
           IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST update) -> NTSTATUS {
        NTSTATUS status = UpdateActiveVidPnPresentPath(context, update);
        DebugTrace("vmmcdisp: UpdateActiveVidPnPresentPath = %lx\n", status);
        return status;
    };
    init.DxgkDdiRecommendMonitorModes =
        [](IN_CONST_HANDLE context,
           IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST query) -> NTSTATUS {
        NTSTATUS status = RecommendMonitorModes(context, query);
        DebugTrace("vmmcdisp: RecommendMonitorModes = %lx\n", status);
        return status;
    };
    init.DxgkDdiSetPowerState = [](IN_CONST_PVOID context, IN_ULONG device,
                                   IN_DEVICE_POWER_STATE state, IN_POWER_ACTION action) -> NTSTATUS {
        DebugTrace("vmmcdisp: SetPowerState(device %lx, D%d)\n", device, state - 1);
        return SetPowerState(context, device, state, action);
    };
    init.DxgkDdiStopDeviceAndReleasePostDisplayOwnership =
        [](PVOID context, D3DDDI_VIDEO_PRESENT_TARGET_ID target,
           PDXGK_DISPLAY_INFORMATION display) -> NTSTATUS {
        DebugTrace("vmmcdisp: StopDeviceAndReleasePostDisplayOwnership\n");
        return StopDeviceAndReleasePostDisplayOwnership(context, target, display);
    };
#endif
    NTSTATUS status = DxgkInitializeDisplayOnlyDriver(driver, registry, &init);
    DebugTrace("vmmcdisp: DriverEntry = %lx\n", status);
    return status;
}
