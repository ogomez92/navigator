//! Eject a volume, and find out who is holding it when that fails.
//!
//! Two mechanisms, chosen by what the drive is:
//!
//! - **PnP eject** (`CM_Request_Device_EjectW`) for disks. This is what
//!   "Safely Remove Hardware" and Explorer's Eject do for a USB stick or an
//!   external SSD: the volume is resolved to its disk device node, the
//!   removable node (usually the USB mass-storage *parent*) is asked to
//!   leave, and the PnP manager flushes, dismounts and powers it down. It
//!   is the only method that works for a USB disk Windows classifies as
//!   *fixed* — a Samsung T7, most external SSDs/HDDs — where
//!   `IOCTL_STORAGE_EJECT_MEDIA` answers "incorrect function" because
//!   there is no media to eject from the drive.
//! - **Media eject** (lock → dismount → `IOCTL_STORAGE_EJECT_MEDIA`) for
//!   optical drives, where "eject" means "open the tray", and as the
//!   fallback when the disk's device node cannot be located.
//!
//! Both fail when something has a file open on the volume, and the PnP
//! veto names a *device*, not the program. [`processes_using_drive`]
//! answers that: it walks the system handle table and reports every
//! process holding a file under the drive, so the dialog can say
//! "Notepad has D:\notes.txt open" instead of "device in use".

use std::ffi::c_void;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DN_REMOVABLE, PNP_VETO_TYPE, PNP_VetoTypeUnknown,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED,
    ERROR_INVALID_FUNCTION, ERROR_NOT_READY, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, GetDriveTypeW, GetFileType,
    GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_EJECT_MEDIA,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    STORAGE_DEVICE_NUMBER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::ErrorModeGuard;

/// `GetDriveTypeW` → optical drive. Inlined like the other `DRIVE_*`
/// constants in `lib.rs` (they live in a windows-sys feature nothing else
/// here needs).
const DRIVE_CDROM: u32 = 5;

/// Why an eject did not happen, in words the user can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectFailure {
    /// One sentence: what was attempted and what refused.
    pub summary: String,
    /// The PnP veto, when there was one: the veto class plus whatever the
    /// PnP manager named (a device instance path, a service, a window).
    pub veto: Option<String>,
}

impl EjectFailure {
    fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            veto: None,
        }
    }
}

/// A process holding files under the ejecting drive.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DriveUser {
    pub pid: u32,
    /// Executable name (`notepad.exe`), or `pid N` when it could not be read.
    pub name: String,
    /// True when the executable itself lives on the drive.
    pub runs_from_drive: bool,
    /// Paths it has open there, capped at [`MAX_PATHS_PER_PROCESS`].
    pub paths: Vec<String>,
    /// How many open paths were seen in total (so the UI can say "and 12 more").
    pub total_paths: usize,
}

pub const MAX_PATHS_PER_PROCESS: usize = 4;

pub(crate) struct Handle(pub(crate) HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn drive_letter(root: &str) -> Result<char, EjectFailure> {
    root.trim()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .ok_or_else(|| EjectFailure::new(format!("{root:?} is not a drive")))
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open `\\.\X:` (or any device path) with the given access, sharing
/// everything — a query handle that must not itself block an eject.
pub(crate) fn open_device(path: &str, access: u32) -> Result<Handle, u32> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

pub(crate) fn ioctl(
    h: &Handle,
    code: u32,
    input: *const c_void,
    input_len: u32,
    out: *mut c_void,
    out_len: u32,
) -> Result<(), u32> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            input,
            input_len,
            out,
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(unsafe { GetLastError() })
    }
}

pub(crate) fn device_number(h: &Handle) -> Result<STORAGE_DEVICE_NUMBER, u32> {
    let mut sdn = STORAGE_DEVICE_NUMBER {
        DeviceType: 0,
        DeviceNumber: 0,
        PartitionNumber: 0,
    };
    ioctl(
        h,
        IOCTL_STORAGE_GET_DEVICE_NUMBER,
        std::ptr::null(),
        0,
        (&raw mut sdn).cast(),
        std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
    )?;
    Ok(sdn)
}

/// Eject the volume mounted at `root` (`"E:\"`, with or without the
/// trailing separator). Synchronous and slow (seconds, with retries) —
/// call it from a worker, never the message pump.
pub fn eject_volume(root: &str) -> Result<(), EjectFailure> {
    let letter = drive_letter(root)?;
    // An empty bay must answer "not ready", not put up a system modal.
    let _quiet = ErrorModeGuard::new();

    let root_w = wide(&format!("{letter}:\\"));
    let drive_type = unsafe { GetDriveTypeW(root_w.as_ptr()) };
    if drive_type == DRIVE_CDROM {
        return eject_media(letter);
    }

    // Resolve the volume to its disk device and ask PnP to eject it.
    let dev = match open_device(&format!("\\\\.\\{letter}:"), 0) {
        Ok(h) => h,
        Err(ERROR_NOT_READY) => {
            return Err(EjectFailure::new(format!(
                "{letter}: no media in the drive"
            )));
        }
        Err(code) => {
            return Err(EjectFailure::new(format!(
                "{letter}: cannot open the volume (error {code})"
            )));
        }
    };
    let sdn = device_number(&dev).map_err(|code| {
        EjectFailure::new(format!("{letter}: cannot identify the disk (error {code})"))
    })?;
    drop(dev);

    let guid = &GUID_DEVINTERFACE_DISK;
    let Some(devinst) = find_devinst(guid, sdn.DeviceNumber) else {
        // No device node for it (a virtual disk, a mount without a PnP
        // disk behind it). Media eject is the only thing left to try.
        return eject_media(letter);
    };
    eject_devnode(letter, devinst)
}

/// Walk the disk (or cdrom) device interfaces and return the device
/// instance whose `STORAGE_DEVICE_NUMBER` matches `devnum`.
pub(crate) fn find_devinst(guid: &windows_sys::core::GUID, devnum: u32) -> Option<u32> {
    let set = unsafe {
        SetupDiGetClassDevsW(
            guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    // `HDEVINFO` is an `isize` in windows-sys; -1 is its INVALID_HANDLE_VALUE.
    if set == -1 || set == 0 {
        return None;
    }
    struct Set(isize);
    impl Drop for Set {
        fn drop(&mut self) {
            unsafe {
                SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
    let set = Set(set);

    let mut index = 0u32;
    loop {
        let mut ifdata = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            InterfaceClassGuid: windows_sys::core::GUID::from_u128(0),
            Flags: 0,
            Reserved: 0,
        };
        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(set.0, std::ptr::null(), guid, index, &mut ifdata)
        };
        if ok == 0 {
            return None;
        }
        index += 1;

        let mut required = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &ifdata,
                std::ptr::null_mut(),
                0,
                &mut required,
                std::ptr::null_mut(),
            );
        }
        if required == 0 {
            continue;
        }
        // The detail struct is a header followed by an inline wide string;
        // allocate it as u64s so the header stays aligned.
        let mut buf: Vec<u64> = vec![0; (required as usize).div_ceil(8) + 1];
        let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe {
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }
        let mut info = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ClassGuid: windows_sys::core::GUID::from_u128(0),
            DevInst: 0,
            Reserved: 0,
        };
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &ifdata,
                detail,
                required,
                std::ptr::null_mut(),
                &mut info,
            )
        };
        if ok == 0 {
            continue;
        }
        let path_ptr = unsafe { (&raw const (*detail).DevicePath).cast::<u16>() };
        let mut len = 0usize;
        while unsafe { *path_ptr.add(len) } != 0 {
            len += 1;
        }
        let path = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(path_ptr, len) });

        let Ok(h) = open_device(&path, 0) else {
            continue;
        };
        if let Ok(sdn) = device_number(&h)
            && sdn.DeviceNumber == devnum
        {
            return Some(info.DevInst);
        }
    }
}

fn is_removable(devinst: u32) -> bool {
    let mut status = 0u32;
    let mut problem = 0u32;
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    cr == CR_SUCCESS && status & DN_REMOVABLE != 0
}

/// Ask the PnP manager to eject the removable node behind `devinst`. For a
/// USB disk the disk node itself is not removable; its parent (the USB
/// mass-storage device) is, and that is what Explorer ejects.
fn eject_devnode(letter: char, devinst: u32) -> Result<(), EjectFailure> {
    let mut parent = 0u32;
    let has_parent = unsafe { CM_Get_Parent(&mut parent, devinst, 0) } == CR_SUCCESS;
    let target = if has_parent && is_removable(parent) {
        parent
    } else if is_removable(devinst) {
        devinst
    } else {
        return Err(EjectFailure::new(format!(
            "{letter}: the device is not removable — Windows does not allow it to be ejected"
        )));
    };

    const TRIES: u32 = 3;
    let mut last = EjectFailure::new(format!("{letter}: eject was refused"));
    for attempt in 0..TRIES {
        let mut veto_type: PNP_VETO_TYPE = PNP_VetoTypeUnknown;
        let mut veto_name = [0u16; MAX_PATH as usize];
        let cr = unsafe {
            CM_Request_Device_EjectW(target, &mut veto_type, veto_name.as_mut_ptr(), MAX_PATH, 0)
        };
        if cr == CR_SUCCESS && veto_type == PNP_VetoTypeUnknown {
            return Ok(());
        }
        let name_len = veto_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(veto_name.len());
        let name = String::from_utf16_lossy(&veto_name[..name_len]);
        last = EjectFailure {
            summary: if veto_type == PNP_VetoTypeUnknown {
                format!("{letter}: eject request failed (CONFIGRET {cr})")
            } else {
                format!("{letter}: {}", veto_sentence(veto_type))
            },
            veto: Some(if name.is_empty() {
                veto_type_name(veto_type).to_string()
            } else {
                format!("{}: {}", veto_type_name(veto_type), name)
            }),
        };
        if attempt + 1 < TRIES {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    Err(last)
}

fn veto_type_name(t: PNP_VETO_TYPE) -> &'static str {
    match t {
        1 => "LegacyDevice",
        2 => "PendingClose",
        3 => "WindowsApp",
        4 => "WindowsService",
        5 => "OutstandingOpen",
        6 => "Device",
        7 => "Driver",
        8 => "IllegalDeviceRequest",
        9 => "InsufficientPower",
        10 => "NonDisableable",
        11 => "LegacyDriver",
        12 => "InsufficientRights",
        13 => "AlreadyRemoved",
        _ => "Unknown",
    }
}

fn veto_sentence(t: PNP_VETO_TYPE) -> &'static str {
    match t {
        2 | 5 => "the drive is in use — a program still has files open on it",
        3 => "a program refused to let the device go",
        4 => "a Windows service refused to let the device go",
        6 | 7 | 11 | 1 => "the device or its driver refused the eject",
        8 => "the device does not accept eject requests",
        9 => "insufficient power to complete the eject",
        10 => "the device cannot be disabled",
        12 => "insufficient rights to eject the device",
        13 => "the device was already removed",
        _ => "eject was refused",
    }
}

/// Lock → dismount → allow removal → eject media. The tray-opening path
/// for optical drives, and the fallback for a disk without a PnP node.
fn eject_media(letter: char) -> Result<(), EjectFailure> {
    let h = match open_device(&format!("\\\\.\\{letter}:"), GENERIC_READ | GENERIC_WRITE) {
        Ok(h) => h,
        Err(ERROR_ACCESS_DENIED) => {
            return Err(EjectFailure::new(format!(
                "{letter}: access denied — opening the volume needs permission"
            )));
        }
        Err(ERROR_NOT_READY) => {
            return Err(EjectFailure::new(format!(
                "{letter}: no media in the drive"
            )));
        }
        Err(code) => {
            return Err(EjectFailure::new(format!(
                "{letter}: cannot open volume (error {code})"
            )));
        }
    };

    const LOCK_ATTEMPTS: u32 = 10;
    let mut locked = false;
    let mut last = 0u32;
    for attempt in 0..LOCK_ATTEMPTS {
        match ioctl(
            &h,
            FSCTL_LOCK_VOLUME,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
        ) {
            Ok(()) => {
                locked = true;
                break;
            }
            Err(code) => {
                last = code;
                if attempt + 1 < LOCK_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }
    if !locked {
        return Err(match last {
            ERROR_ACCESS_DENIED => EjectFailure::new(format!(
                "{letter}: drive is in use — a program still has files open on it"
            )),
            _ => EjectFailure::new(format!(
                "{letter}: could not lock the volume (error {last})"
            )),
        });
    }

    ioctl(
        &h,
        FSCTL_DISMOUNT_VOLUME,
        std::ptr::null(),
        0,
        std::ptr::null_mut(),
        0,
    )
    .map_err(|code| EjectFailure::new(format!("{letter}: dismount failed (error {code})")))?;

    let prevent = PREVENT_MEDIA_REMOVAL {
        PreventMediaRemoval: false,
    };
    // Not every device implements removal prevention; the eject itself is
    // the verdict.
    let _ = ioctl(
        &h,
        IOCTL_STORAGE_MEDIA_REMOVAL,
        (&raw const prevent).cast(),
        std::mem::size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
        std::ptr::null_mut(),
        0,
    );

    ioctl(&h, IOCTL_STORAGE_EJECT_MEDIA, std::ptr::null(), 0, std::ptr::null_mut(), 0).map_err(|code| {
        match code {
            ERROR_INVALID_FUNCTION => EjectFailure::new(format!(
                "{letter}: the device has no ejectable media and no removable device node — volume was dismounted"
            )),
            _ => EjectFailure::new(format!("{letter}: eject failed (error {code})")),
        }
    })
}

// ---- who is using the drive -------------------------------------------

/// Whether this process runs with an elevated (administrator) token. The
/// handle scan can only look into processes at or below its own level, so
/// an empty result from an unelevated scan is not the same as "nobody".
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut tok: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
        return false;
    }
    let tok = Handle(tok);
    let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            tok.0,
            TokenElevation,
            (&raw mut el).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    ok != 0 && el.TokenIsElevated != 0
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

const SYSTEM_EXTENDED_HANDLE_INFORMATION: u32 = 64;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

/// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX` — undocumented but unchanged since
/// Vista, and what every "what has this file open" tool reads.
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntry {
    object: *mut c_void,
    pid: usize,
    handle: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

#[repr(C)]
struct HandleInfoHeader {
    count: usize,
    reserved: usize,
}

fn system_handles() -> Option<Vec<HandleEntry>> {
    let mut size: u32 = 1 << 20;
    for _ in 0..8 {
        let mut buf: Vec<u8> = vec![0; size as usize];
        let mut needed = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_EXTENDED_HANDLE_INFORMATION,
                buf.as_mut_ptr().cast(),
                size,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            size = needed.max(size.saturating_mul(2));
            continue;
        }
        if status < 0 {
            return None;
        }
        let header = unsafe { buf.as_ptr().cast::<HandleInfoHeader>().read_unaligned() };
        let base = unsafe { buf.as_ptr().add(std::mem::size_of::<HandleInfoHeader>()) };
        let max = (buf.len() - std::mem::size_of::<HandleInfoHeader>())
            / std::mem::size_of::<HandleEntry>();
        let n = header.count.min(max);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(unsafe { base.cast::<HandleEntry>().add(i).read_unaligned() });
        }
        return Some(out);
    }
    None
}

/// Access masks that are known to belong to synchronous pipe / console
/// handles on which name queries can block forever. Skipping them is the
/// standard precaution.
fn is_hazardous_access(access: u32) -> bool {
    matches!(
        access,
        0x0012_019f | 0x001a_019f | 0x0012_0189 | 0x0010_0000
    )
}

fn process_image(pid: u32) -> Option<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let h = Handle(h);
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok =
        unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

fn starts_with_drive(path: &str, letter: char) -> bool {
    let mut chars = path.chars();
    matches!(chars.next(), Some(c) if c.eq_ignore_ascii_case(&letter))
        && chars.next() == Some(':')
        && matches!(chars.next(), None | Some('\\'))
}

/// Every process holding a file under `root`, with the paths it holds.
///
/// Walks the system handle table once, duplicates each *file* handle of
/// every other process into ours, and asks the kernel for its final path.
/// Only `FILE_TYPE_DISK` handles are named — the name query on a pipe can
/// block — and processes we cannot open (protected, other users) are
/// skipped rather than failed. Also lists a process whose executable
/// itself lives on the drive, since that holds the volume without any
/// handle of its own. Costs a few hundred milliseconds on a busy machine;
/// worker thread only.
pub fn processes_using_drive(root: &str) -> Vec<DriveUser> {
    let Ok(letter) = drive_letter(root) else {
        return Vec::new();
    };
    // Which object-type index means "File" on this Windows build? Open a
    // file ourselves and look our own handle up in the table — so the
    // probe must be open *before* the snapshot is taken.
    let probe = std::fs::File::open(std::env::current_exe().unwrap_or_default())
        .ok()
        .or_else(|| std::fs::File::open("C:\\").ok());
    let Some(entries) = system_handles() else {
        return Vec::new();
    };
    let me = unsafe { GetCurrentProcessId() } as usize;
    let file_type_index = probe.as_ref().and_then(|f| {
        use std::os::windows::io::AsRawHandle;
        let raw = f.as_raw_handle() as usize;
        entries
            .iter()
            .find(|e| e.pid == me && e.handle == raw)
            .map(|e| e.object_type_index)
    });
    let Some(file_type_index) = file_type_index else {
        return Vec::new();
    };
    drop(probe);

    use std::collections::BTreeMap;
    let mut open: BTreeMap<usize, Option<Handle>> = BTreeMap::new();
    let mut users: BTreeMap<usize, DriveUser> = BTreeMap::new();
    let this = unsafe { GetCurrentProcess() };

    for e in entries.iter() {
        if e.pid == me || e.pid == 0 || e.pid == 4 {
            continue;
        }
        if e.object_type_index != file_type_index || is_hazardous_access(e.granted_access) {
            continue;
        }
        let proc_handle = open.entry(e.pid).or_insert_with(|| {
            let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, e.pid as u32) };
            if h.is_null() { None } else { Some(Handle(h)) }
        });
        let Some(proc_handle) = proc_handle.as_ref() else {
            continue;
        };
        let mut dup: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            DuplicateHandle(
                proc_handle.0,
                e.handle as HANDLE,
                this,
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 || dup.is_null() {
            continue;
        }
        let dup = Handle(dup);
        if unsafe { GetFileType(dup.0) } != FILE_TYPE_DISK {
            continue;
        }
        let mut buf = [0u16; 1024];
        let len = unsafe {
            GetFinalPathNameByHandleW(dup.0, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS)
        };
        if len == 0 || len as usize >= buf.len() {
            continue;
        }
        let mut path = String::from_utf16_lossy(&buf[..len as usize]);
        if let Some(stripped) = path.strip_prefix("\\\\?\\") {
            path = stripped.to_string();
        }
        if !starts_with_drive(&path, letter) {
            continue;
        }
        let user = users.entry(e.pid).or_insert_with(|| {
            let image = process_image(e.pid as u32);
            DriveUser {
                pid: e.pid as u32,
                name: image
                    .as_deref()
                    .and_then(|p| p.rsplit('\\').next())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("pid {}", e.pid)),
                runs_from_drive: image
                    .as_deref()
                    .is_some_and(|p| starts_with_drive(p, letter)),
                paths: Vec::new(),
                total_paths: 0,
            }
        });
        if !user.paths.contains(&path) {
            user.total_paths += 1;
            if user.paths.len() < MAX_PATHS_PER_PROCESS {
                user.paths.push(path);
            }
        }
    }

    // Processes running *from* the drive hold it without a file handle we
    // can see; add them from the pids we already touched.
    for pid in open.keys() {
        if users.contains_key(pid) {
            continue;
        }
        if let Some(image) = process_image(*pid as u32)
            && starts_with_drive(&image, letter)
        {
            users.insert(
                *pid,
                DriveUser {
                    pid: *pid as u32,
                    name: image.rsplit('\\').next().unwrap_or(&image).to_string(),
                    runs_from_drive: true,
                    paths: vec![image.clone()],
                    total_paths: 1,
                },
            );
        }
    }

    users.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_prefix_match_is_case_insensitive_and_exact() {
        assert!(starts_with_drive("D:\\foo\\bar.txt", 'D'));
        assert!(starts_with_drive("d:\\foo", 'D'));
        assert!(starts_with_drive("D:", 'd'));
        assert!(!starts_with_drive("DD:\\x", 'D'));
        assert!(!starts_with_drive("C:\\D:\\x", 'D'));
        assert!(!starts_with_drive("", 'D'));
    }

    #[test]
    fn drive_letter_parses_roots_and_rejects_junk() {
        assert_eq!(drive_letter("d:\\").unwrap(), 'D');
        assert_eq!(drive_letter("E:").unwrap(), 'E');
        assert!(drive_letter("\\\\host\\share").is_err());
        assert!(drive_letter("").is_err());
    }

    #[test]
    fn our_own_open_file_is_found_in_the_handle_table() {
        // Pins the file-type-index discovery the scan depends on: if the
        // handle table cannot be read, `processes_using_drive` silently
        // returns nothing, which is the one failure that must not hide.
        let f = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        use std::os::windows::io::AsRawHandle;
        let raw = f.as_raw_handle() as usize;
        let me = unsafe { GetCurrentProcessId() } as usize;
        let entries = system_handles().expect("handle table readable");
        assert!(entries.iter().any(|e| e.pid == me && e.handle == raw));
    }

    #[test]
    fn a_file_we_hold_open_shows_us_as_a_user_of_its_drive() {
        // Scan from a *child* process's point of view is not available in
        // a unit test, but the scan skips our own pid on purpose, so hold
        // the file from a second process: a sleeping cmd with a redirect.
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held.txt");
        std::fs::write(&held, b"x").unwrap();
        // The scan skips our own pid on purpose, so hold the file from a
        // second process: PowerShell opens it, drops a marker file so the
        // test knows the handle exists (PowerShell can take seconds to
        // start), then keeps the stream open.
        let marker = dir.path().join("ready");
        let mut child = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "$f=[IO.File]::Open('{}','Open','Read','None'); Set-Content -Path '{}' -Value 1; Start-Sleep 15; $f.Close()",
                    held.display(),
                    marker.display()
                ),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn holder");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(marker.exists(), "holder never signalled readiness");
        let root: String = held.to_string_lossy().chars().take(3).collect();
        let users = processes_using_drive(&root);
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        // The per-process path list is capped, and PowerShell holds a dozen
        // files on C: before ours, so assert on the pid rather than on the
        // held path being among the first few listed.
        let holder = users.iter().find(|u| u.pid == pid);
        assert!(
            holder.is_some_and(|u| u.total_paths >= 1),
            "holder pid {pid} not reported; {} users found",
            users.len()
        );
    }
}
