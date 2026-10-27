//! How a volume's disk is attached: the storage bus, and for a USB disk,
//! which port it is on and how fast the link came up.
//!
//! Everything here is a *query*. The volume is opened with no access
//! rights, the hub with write access only because that is what the USB
//! hub driver demands of `DeviceIoControl` callers (usbview does the
//! same, unelevated), and no IOCTL issued here changes device state — in
//! particular nothing locks, dismounts or ejects. The eject path shares
//! the volume → disk device-node resolution and nothing else.
//!
//! The port question has three separate answers, and conflating them is
//! how "USB 3" lies:
//!
//! - **What the link is running at** — `USB_NODE_CONNECTION_INFORMATION_EX`
//!   `.Speed`, refined by the V2 query for SuperSpeedPlus.
//! - **What the device could do** — the V2 flags
//!   (`DeviceIsSuperSpeedCapableOrHigher` and the Plus variant). A USB 3
//!   drive plugged into a USB 2.0 port enumerates as a High-speed device
//!   whose descriptor says USB 2.1; only these flags still know it is a
//!   USB 3 drive.
//! - **What the port could do** — the V2 `SupportedUsbProtocols` names the
//!   protocols of the port the device is on, and for a port that only
//!   speaks USB 2.0, `IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES` says whether
//!   it has a SuperSpeed *companion*: on xHCI a USB 3 connector is two
//!   logical ports (a 2.0 one and a 3.x one) paired by that link, so a
//!   2.0 port with a companion is a USB 3 connector that the device, or
//!   its cable, failed to use, and one without is a real USB 2.0 port.

use std::ffi::c_void;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DRP_ADDRESS, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_DevNode_Registry_PropertyW,
    CM_Get_Device_ID_Size, CM_Get_Device_IDW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_Get_Parent, CR_SUCCESS,
};
use windows_sys::Win32::Devices::Usb::{
    GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
    IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2, IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES,
    USB_NODE_CONNECTION_INFORMATION_EX, USB_NODE_CONNECTION_INFORMATION_EX_V2, USB_PIPE_INFO,
    USB_PORT_CONNECTOR_PROPERTIES, USB_PROTOCOLS,
};
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::{
    BusType1394, BusTypeAta, BusTypeAtapi, BusTypeFibre, BusTypeFileBackedVirtual, BusTypeMmc,
    BusTypeNvme, BusTypeRAID, BusTypeSCM, BusTypeSas, BusTypeSata, BusTypeScsi, BusTypeSd,
    BusTypeSpaces, BusTypeUfs, BusTypeUsb, BusTypeVirtual, BusTypeiScsi,
};
use windows_sys::Win32::System::Ioctl::{
    GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};

use super::ErrorModeGuard;
use super::eject::{Handle, device_number, find_devinst, ioctl, open_device, wide};

/// How a volume's disk is attached to the computer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriveConnection {
    /// Storage bus word: `USB`, `NVMe`, `SATA`, … Empty when the disk
    /// would not answer the property query at all.
    pub bus: String,
    /// Vendor + product from the device descriptor (`Samsung PSSD T7
    /// Shield`). Empty when unknown.
    pub product: String,
    /// The USB port and link, for a USB disk whose hub could be queried.
    pub usb: Option<UsbLink>,
}

/// USB signalling rates, slowest first. `Ord` so "could do more than it
/// is doing" is a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UsbSpeed {
    /// 1.5 Mbps (USB 1.0).
    Low,
    /// 12 Mbps (USB 1.1).
    Full,
    /// 480 Mbps (USB 2.0).
    High,
    /// 5 Gbps (USB 3.0 / 3.1 Gen 1 / 3.2 Gen 1).
    Super,
    /// 10 Gbps or more (USB 3.1 Gen 2 / 3.2 Gen 2 / Gen 2x2). The hub
    /// driver does not distinguish 10 from 20.
    SuperPlus,
}

impl UsbSpeed {
    /// `"5 Gbps"`, `"480 Mbps"`, …
    pub fn rate(self) -> &'static str {
        match self {
            UsbSpeed::Low => "1.5 Mbps",
            UsbSpeed::Full => "12 Mbps",
            UsbSpeed::High => "480 Mbps",
            UsbSpeed::Super => "5 Gbps",
            UsbSpeed::SuperPlus => "10 Gbps or more",
        }
    }

    /// The USB-IF signalling name.
    pub fn name(self) -> &'static str {
        match self {
            UsbSpeed::Low => "Low speed",
            UsbSpeed::Full => "Full speed",
            UsbSpeed::High => "High speed",
            UsbSpeed::Super => "SuperSpeed",
            UsbSpeed::SuperPlus => "SuperSpeedPlus",
        }
    }

    /// Whether this is a USB 3 rate.
    pub fn is_usb3(self) -> bool {
        self >= UsbSpeed::Super
    }
}

/// The port a USB disk is on and the link it negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbLink {
    /// What the link is running at.
    pub speed: UsbSpeed,
    /// The fastest the *device* can do, when the hub driver reports it
    /// (it does on every xHCI stack; `None` on a hub that refused the V2
    /// query).
    pub device_max: Option<UsbSpeed>,
    /// Whether the physical connector supports USB 3. `None` when the hub
    /// answered neither the protocol nor the companion query.
    pub port_usb3: Option<bool>,
    /// 1-based port number on its hub.
    pub port: u32,
    /// True when the hub driver knows the connector is USB-C.
    pub type_c: bool,
    /// External hubs between the device and the computer's own
    /// controller. 0 means a port on the computer itself.
    pub hubs_between: u32,
    /// `bcdUSB` from the device descriptor, e.g. `0x0320` for USB 3.2.
    /// This is what the device *claims*; a USB 3 drive on a 2.0 port
    /// claims 2.1, so it only refines a USB 3 link.
    pub bcd_usb: u16,
}

impl UsbLink {
    /// The USB version the link is running as, in the words the box the
    /// drive came in would use — `USB 3.2 Gen 2`, `USB 2.0` — combining
    /// the negotiated rate with the version the device descriptor claims.
    pub fn version_label(&self) -> String {
        let major = (self.bcd_usb >> 8) & 0xff;
        let minor = (self.bcd_usb >> 4) & 0xf;
        match self.speed {
            UsbSpeed::SuperPlus => match (major, minor) {
                (3, 1) => "USB 3.1 Gen 2".into(),
                (3, m) if m >= 2 => "USB 3.2 Gen 2".into(),
                _ => "USB 3.x Gen 2".into(),
            },
            UsbSpeed::Super => match (major, minor) {
                (3, 0) => "USB 3.0".into(),
                (3, 1) => "USB 3.1 Gen 1".into(),
                (3, m) if m >= 2 => "USB 3.2 Gen 1".into(),
                _ => "USB 3.x Gen 1".into(),
            },
            UsbSpeed::High => "USB 2.0".into(),
            UsbSpeed::Full => "USB 1.1".into(),
            UsbSpeed::Low => "USB 1.0".into(),
        }
    }
}

fn bus_word(bus: i32) -> &'static str {
    match bus {
        b if b == BusTypeUsb => "USB",
        b if b == BusTypeNvme => "NVMe",
        b if b == BusTypeSata => "SATA",
        b if b == BusTypeAta => "ATA",
        b if b == BusTypeAtapi => "ATAPI",
        b if b == BusTypeScsi => "SCSI",
        b if b == BusTypeSas => "SAS",
        b if b == BusTypeiScsi => "iSCSI",
        b if b == BusTypeRAID => "RAID",
        b if b == BusTypeSd => "SD card",
        b if b == BusTypeMmc => "MMC",
        b if b == BusTypeUfs => "UFS",
        b if b == BusType1394 => "FireWire",
        b if b == BusTypeFibre => "Fibre Channel",
        b if b == BusTypeSCM => "storage-class memory",
        b if b == BusTypeSpaces => "Storage Spaces",
        b if b == BusTypeVirtual => "virtual",
        b if b == BusTypeFileBackedVirtual => "virtual (file-backed)",
        _ => "unknown",
    }
}

/// Query how the volume at `root` (`"D:\"`) is attached. Slow enough for
/// a worker (SetupAPI enumerates every disk, and each is opened to match
/// the device number); never call it from the message pump.
pub fn drive_connection(root: &str) -> Option<DriveConnection> {
    let letter = root.trim().chars().next()?.to_ascii_uppercase();
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let _quiet = ErrorModeGuard::new();

    let dev = open_device(&format!("\\\\.\\{letter}:"), 0).ok()?;
    let mut conn = DriveConnection::default();
    let mut bus = -1;
    if let Some(desc) = device_descriptor(&dev) {
        conn.bus = bus_word(desc.0).into();
        conn.product = desc.1;
        bus = desc.0;
    }
    if bus != BusTypeUsb {
        return Some(conn);
    }
    let Ok(sdn) = device_number(&dev) else {
        return Some(conn);
    };
    drop(dev);

    if let Some(devinst) = find_devinst(&GUID_DEVINTERFACE_DISK, sdn.DeviceNumber) {
        conn.usb = usb_link(devinst);
    }
    Some(conn)
}

/// `(bus type, "Vendor Product")` from `IOCTL_STORAGE_QUERY_PROPERTY`.
fn device_descriptor(h: &Handle) -> Option<(i32, String)> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // First call sizes it; the descriptor is a header followed by the
    // vendor / product / serial strings at the offsets it names.
    let mut header = STORAGE_DEVICE_DESCRIPTOR::default();
    ioctl(
        h,
        IOCTL_STORAGE_QUERY_PROPERTY,
        (&raw const query).cast(),
        std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
        (&raw mut header).cast(),
        std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
    )
    .ok()?;
    let size = (header.Size as usize).max(std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>());
    let mut buf: Vec<u64> = vec![0; size.div_ceil(8) + 1];
    ioctl(
        h,
        IOCTL_STORAGE_QUERY_PROPERTY,
        (&raw const query).cast(),
        std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
        buf.as_mut_ptr().cast(),
        (buf.len() * 8) as u32,
    )
    .ok()?;
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
    let desc =
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>()) };
    let ansi_at = |off: u32| -> String {
        let off = off as usize;
        if off == 0 || off >= bytes.len() {
            return String::new();
        }
        let end = bytes[off..]
            .iter()
            .position(|&b| b == 0)
            .map_or(bytes.len(), |p| off + p);
        String::from_utf8_lossy(&bytes[off..end]).trim().to_string()
    };
    let vendor = ansi_at(desc.VendorIdOffset);
    let product = ansi_at(desc.ProductIdOffset);
    let name = if product.is_empty() {
        vendor
    } else if vendor.is_empty() || product.to_lowercase().starts_with(&vendor.to_lowercase()) {
        product
    } else {
        format!("{vendor} {product}")
    };
    Some((desc.BusType, name))
}

/// Device instance ID (`USB\VID_04E8&PID_61FB\…`) of a devnode.
fn instance_id(devinst: u32) -> Option<String> {
    let mut len = 0u32;
    if unsafe { CM_Get_Device_ID_Size(&mut len, devinst, 0) } != CR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    if unsafe { CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), buf.len() as u32, 0) } != CR_SUCCESS {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The `\\?\USB#…#{hub guid}` path of a devnode that is a USB hub, or
/// `None` when it isn't one.
fn hub_interface_path(instance: &str) -> Option<String> {
    let id = wide(instance);
    let mut len = 0u32;
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &GUID_DEVINTERFACE_USB_HUB,
            id.as_ptr(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len < 2 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_USB_HUB,
            id.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// `CM_DRP_ADDRESS`: for a USB device, its port number on the hub.
fn port_number(devinst: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    let mut reg_type = 0u32;
    let cr = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            devinst,
            CM_DRP_ADDRESS,
            &mut reg_type,
            (&raw mut value).cast::<c_void>(),
            &mut len,
            0,
        )
    };
    (cr == CR_SUCCESS && value > 0).then_some(value)
}

fn is_root_hub(instance: &str) -> bool {
    instance.to_ascii_uppercase().starts_with("USB\\ROOT_HUB")
}

/// Walk up from the disk devnode to the first USB hub, remember the
/// child we came from (the USB device, i.e. the port's occupant), then
/// keep walking to count external hubs up to the root hub.
fn usb_link(disk_devinst: u32) -> Option<UsbLink> {
    let mut child = disk_devinst;
    let mut hub = None;
    let mut cur = disk_devinst;
    for _ in 0..16 {
        let mut parent = 0u32;
        if unsafe { CM_Get_Parent(&mut parent, cur, 0) } != CR_SUCCESS {
            return None;
        }
        let id = instance_id(parent)?;
        if let Some(path) = hub_interface_path(&id) {
            hub = Some((parent, id, path));
            break;
        }
        child = parent;
        cur = parent;
    }
    let (hub_devinst, hub_id, hub_path) = hub?;
    let port = port_number(child)?;

    // Hubs between the device and the computer: this one, if it is not
    // the root hub, plus every further hub up the chain.
    let mut hubs_between = 0u32;
    let mut cur = hub_devinst;
    let mut id = hub_id;
    for _ in 0..16 {
        if is_root_hub(&id) {
            break;
        }
        hubs_between += 1;
        let mut parent = 0u32;
        if unsafe { CM_Get_Parent(&mut parent, cur, 0) } != CR_SUCCESS {
            break;
        }
        let Some(pid) = instance_id(parent) else {
            break;
        };
        if hub_interface_path(&pid).is_none() {
            break;
        }
        cur = parent;
        id = pid;
    }

    // The hub driver wants write access on its handle for these queries
    // (usbview opens hubs the same way, without elevation); nothing here
    // writes to the device. Fall back to a bare handle if that is refused.
    let hub = open_device(&hub_path, GENERIC_WRITE)
        .or_else(|_| open_device(&hub_path, 0))
        .ok()?;

    let info = connection_info(&hub, port)?;
    let mut speed = match info.Speed {
        0 => UsbSpeed::Low,
        1 => UsbSpeed::Full,
        2 => UsbSpeed::High,
        _ => UsbSpeed::Super,
    };
    let bcd_usb = info.DeviceDescriptor.bcdUSB;

    let mut device_max = None;
    let mut port_usb3 = None;
    if let Some((protocols, flags)) = connection_info_v2(&hub, port) {
        // USB_PROTOCOLS: bit 0 Usb110, bit 1 Usb200, bit 2 Usb300.
        if protocols & 0b100 != 0 {
            port_usb3 = Some(true);
        }
        // Flags: bit 0 operating at SS or higher, bit 1 SS capable or
        // higher, bit 2 operating at SS+ or higher, bit 3 SS+ capable.
        if flags & 0b100 != 0 {
            speed = UsbSpeed::SuperPlus;
        } else if flags & 0b1 != 0 {
            speed = UsbSpeed::Super;
        }
        device_max = Some(if flags & 0b1000 != 0 {
            UsbSpeed::SuperPlus
        } else if flags & 0b10 != 0 {
            UsbSpeed::Super
        } else {
            speed
        });
    }

    let mut type_c = false;
    if let Some((props, has_companion)) = connector_properties(&hub, port) {
        // USB_PORT_PROPERTIES: bit 3 PortConnectorIsTypeC.
        type_c = props & 0b1000 != 0;
        if port_usb3.is_none() {
            port_usb3 = Some(has_companion || speed.is_usb3());
        }
    }
    if port_usb3.is_none() && speed.is_usb3() {
        port_usb3 = Some(true);
    }

    Some(UsbLink {
        speed,
        device_max,
        port_usb3,
        port,
        type_c,
        hubs_between,
        bcd_usb,
    })
}

fn connection_info(hub: &Handle, port: u32) -> Option<USB_NODE_CONNECTION_INFORMATION_EX> {
    // The struct ends in an open-ended pipe list; usbview reserves room
    // for 30, which no mass-storage device approaches.
    let size = std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX>()
        + 30 * std::mem::size_of::<USB_PIPE_INFO>();
    let mut buf: Vec<u64> = vec![0; size.div_ceil(8) + 1];
    let p = buf
        .as_mut_ptr()
        .cast::<USB_NODE_CONNECTION_INFORMATION_EX>();
    let info = USB_NODE_CONNECTION_INFORMATION_EX {
        ConnectionIndex: port,
        ..Default::default()
    };
    unsafe { std::ptr::write_unaligned(p, info) };
    ioctl(
        hub,
        IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
        p.cast(),
        size as u32,
        p.cast(),
        size as u32,
    )
    .ok()?;
    let info = unsafe { std::ptr::read_unaligned(p) };
    // USB_CONNECTION_STATUS: 1 = DeviceConnected.
    (info.ConnectionStatus == 1).then_some(info)
}

/// `(SupportedUsbProtocols bits, Flags bits)`.
fn connection_info_v2(hub: &Handle, port: u32) -> Option<(u32, u32)> {
    // On input `SupportedUsbProtocols` names the protocols the *caller*
    // understands; the hub driver answers ERROR_INVALID_PARAMETER when it
    // is left zero (usbview sets Usb300, bit 2, the same way).
    let mut v2 = USB_NODE_CONNECTION_INFORMATION_EX_V2 {
        ConnectionIndex: port,
        Length: std::mem::size_of::<USB_NODE_CONNECTION_INFORMATION_EX_V2>() as u32,
        SupportedUsbProtocols: USB_PROTOCOLS { ul: 0b100 },
        ..Default::default()
    };
    ioctl(
        hub,
        IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX_V2,
        (&raw const v2).cast(),
        v2.Length,
        (&raw mut v2).cast(),
        v2.Length,
    )
    .ok()?;
    let protocols = unsafe { v2.SupportedUsbProtocols.ul };
    let flags = unsafe { v2.Flags.ul };
    Some((protocols, flags))
}

/// `(UsbPortProperties bits, has a SuperSpeed companion port)`.
fn connector_properties(hub: &Handle, port: u32) -> Option<(u32, bool)> {
    let header = std::mem::size_of::<USB_PORT_CONNECTOR_PROPERTIES>();
    let mut first = USB_PORT_CONNECTOR_PROPERTIES {
        ConnectionIndex: port,
        ..Default::default()
    };
    ioctl(
        hub,
        IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES,
        (&raw const first).cast(),
        header as u32,
        (&raw mut first).cast(),
        header as u32,
    )
    .ok()?;
    let size = (first.ActualLength as usize).max(header);
    let mut buf: Vec<u64> = vec![0; size.div_ceil(8) + 1];
    let p = buf.as_mut_ptr().cast::<USB_PORT_CONNECTOR_PROPERTIES>();
    unsafe { std::ptr::write_unaligned(p, first) };
    ioctl(
        hub,
        IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES,
        p.cast(),
        size as u32,
        p.cast(),
        size as u32,
    )
    .ok()?;
    let props = unsafe { std::ptr::read_unaligned(p) };
    let bits = unsafe { props.UsbPortProperties.ul };
    // A companion is named by a non-empty symbolic link after the header.
    let link_off =
        std::mem::offset_of!(USB_PORT_CONNECTOR_PROPERTIES, CompanionHubSymbolicLinkName);
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
    let first_char = u16::from_le_bytes([bytes[link_off], bytes[link_off + 1]]);
    let has_companion = props.ActualLength as usize > link_off && first_char != 0;
    Some((bits, has_companion))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(speed: UsbSpeed, bcd: u16) -> UsbLink {
        UsbLink {
            speed,
            device_max: None,
            port_usb3: None,
            port: 1,
            type_c: false,
            hubs_between: 0,
            bcd_usb: bcd,
        }
    }

    #[test]
    fn version_label_combines_rate_with_the_claimed_spec() {
        assert_eq!(
            link(UsbSpeed::SuperPlus, 0x0320).version_label(),
            "USB 3.2 Gen 2"
        );
        assert_eq!(
            link(UsbSpeed::SuperPlus, 0x0310).version_label(),
            "USB 3.1 Gen 2"
        );
        assert_eq!(link(UsbSpeed::Super, 0x0300).version_label(), "USB 3.0");
        assert_eq!(
            link(UsbSpeed::Super, 0x0310).version_label(),
            "USB 3.1 Gen 1"
        );
        assert_eq!(
            link(UsbSpeed::Super, 0x0320).version_label(),
            "USB 3.2 Gen 1"
        );
        // A USB 3 drive on a 2.0 port claims 2.1 — that is still USB 2.0.
        assert_eq!(link(UsbSpeed::High, 0x0210).version_label(), "USB 2.0");
        assert_eq!(link(UsbSpeed::Full, 0x0110).version_label(), "USB 1.1");
    }

    #[test]
    fn speeds_order_slowest_first() {
        assert!(UsbSpeed::High < UsbSpeed::Super);
        assert!(UsbSpeed::Super < UsbSpeed::SuperPlus);
        assert!(!UsbSpeed::High.is_usb3());
        assert!(UsbSpeed::Super.is_usb3());
    }

    #[test]
    fn bus_words_cover_the_common_buses() {
        assert_eq!(bus_word(BusTypeUsb), "USB");
        assert_eq!(bus_word(BusTypeNvme), "NVMe");
        assert_eq!(bus_word(BusTypeSata), "SATA");
        assert_eq!(bus_word(999), "unknown");
    }

    /// Against the live machine: every fixed or removable drive must
    /// answer the bus query, and a USB one must resolve its port. Skipped
    /// where there is no USB disk, so CI without one still passes.
    #[test]
    fn live_drives_answer_the_bus_query() {
        let roots = super::super::list_drives()
            .into_iter()
            .filter_map(|e| super::super::drive_path_from_display(&e.name));
        for root in roots {
            let Some(conn) = drive_connection(&root) else {
                continue;
            };
            eprintln!("{root} -> {conn:?}");
            assert!(!conn.bus.is_empty(), "{root}: bus unknown");
            if conn.bus == "USB" {
                let usb = conn.usb.as_ref().expect("USB disk resolves its port");
                assert!(usb.port > 0);
            }
        }
    }
}
