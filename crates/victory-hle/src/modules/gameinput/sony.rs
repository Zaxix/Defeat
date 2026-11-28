//! PlayStation pads (DualShock 4, DualSense, DualSense Edge) read as HID
//! devices. Windows gives them no XInput slot of their own, so without
//! Steam Input or DS4Windows they were invisible here: the owner's PS5 pad
//! did nothing. They fill the gamepad slots XInput leaves empty, in the
//! order they were found (`VICTORY_SONY_PADS=0` turns this off).
//!
//! Only the input reports are read (sticks, buttons, triggers); no rumble,
//! light bar or motion. The report layouts are the ones SDL's HIDAPI
//! drivers and Linux's `hid-playstation` read.
// Off Windows only the parsing is built (for the tests).
#![cfg_attr(not(windows), allow(dead_code))]

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

/// Sony's USB vendor number.
pub(super) const SONY: u16 = 0x054C;

/// Which pad a product number is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    DualShock4,
    DualSense,
}

pub(super) fn kind(product: u16) -> Option<Kind> {
    match product {
        // DualShock 4 (both models) and its wireless adapter.
        0x05C4 | 0x09CC | 0x0BA0 => Some(Kind::DualShock4),
        // DualSense and DualSense Edge.
        0x0CE6 | 0x0DF2 => Some(Kind::DualSense),
        _ => None,
    }
}

/// The pad's state from one input report as Windows hands it (its first
/// byte the report number); `None` for a report that carries no state.
pub(super) fn parse(kind: Kind, report: &[u8]) -> Option<PadState> {
    let (&id, _) = report.split_first()?;
    // Where the sticks start, and whether the triggers come before the
    // buttons (the DualSense's full report) or after (every other one).
    let (at, full) = match (kind, id) {
        // USB: the full report, 64 bytes. Bluetooth: the short one (10
        // bytes, padded to 78 by Windows) until the pad is asked for more.
        (Kind::DualSense, 0x01) => (1, report.len() == 64),
        // Bluetooth, full: a byte more before the state.
        (Kind::DualSense, 0x31) => (2, true),
        (Kind::DualShock4, 0x01) => (1, false),
        (Kind::DualShock4, 0x11) => (3, false),
        _ => return None,
    };
    let d = report.get(at..at + 10)?;
    let (triggers, buttons) = if full {
        ([d[4], d[5]], [d[7], d[8], d[9]])
    } else {
        ([d[7], d[8]], [d[4], d[5], d[6]])
    };
    Some(state(d[0], d[1], d[2], d[3], triggers, buttons))
}

/// Sticks 0-255 (128 centred, down positive), triggers 0-255, and the three
/// button bytes: the hat and the face buttons, the shoulders and the rest,
/// and the PS button.
fn state(lx: u8, ly: u8, rx: u8, ry: u8, triggers: [u8; 2], b: [u8; 3]) -> PadState {
    let axis = |v: u8| ((f32::from(v) - 128.0) / 127.0).clamp(-1.0, 1.0);
    // The hat: 0 up, then clockwise in eighths; 8 and over centred.
    let hat = match b[0] & 0x0F {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => DOWN | RIGHT,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => UP | LEFT,
        _ => 0,
    };
    let pressed = [
        (b[0] & 0x10, X),              // square
        (b[0] & 0x20, A),              // cross
        (b[0] & 0x40, B),              // circle
        (b[0] & 0x80, Y),              // triangle
        (b[1] & 0x01, LEFT_SHOULDER),  // L1
        (b[1] & 0x02, RIGHT_SHOULDER), // R1
        (b[1] & 0x10, VIEW),           // Share / Create
        (b[1] & 0x20, MENU),           // Options
        (b[1] & 0x40, LEFT_STICK),     // L3
        (b[1] & 0x80, RIGHT_STICK),    // R3
    ];
    PadState {
        buttons: pressed
            .iter()
            .filter(|(on, _)| *on != 0)
            .fold(hat, |all, (_, bit)| all | bit),
        left_trigger: f32::from(triggers[0]) / 255.0,
        right_trigger: f32::from(triggers[1]) / 255.0,
        left_x: axis(lx),
        left_y: -axis(ly),
        right_x: axis(rx),
        right_y: -axis(ry),
    }
}

/// The vendor and product numbers a HID device's path names, for a device
/// that cannot be opened to ask (HidHide, or another program holding it
/// alone): USB paths hold `VID_054C&PID_0CE6`, Bluetooth ones
/// `_VID&0002054C_PID&0CE6` (the `0002` names the Bluetooth numbering).
pub(super) fn path_ids(path: &str) -> Option<(u16, u16)> {
    let lower = path.to_ascii_lowercase();
    let hex4 = |s: &str| s.get(..4).and_then(|h| u16::from_str_radix(h, 16).ok());
    let vendor = if let Some(at) = lower.find("vid_") {
        hex4(&lower[at + 4..])?
    } else {
        let at = lower.find("vid&")?;
        // Four digits of numbering source, then the vendor.
        hex4(lower.get(at + 8..)?)?
    };
    let product = if let Some(at) = lower.find("pid_") {
        hex4(&lower[at + 4..])?
    } else {
        hex4(&lower[lower.find("pid&")? + 4..])?
    };
    Some((vendor, product))
}

/// Whether PlayStation pads are read (`VICTORY_SONY_PADS`, on unless `0`).
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SONY_PADS").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
pub(super) use host::pad;

#[cfg(windows)]
mod host {
    use std::sync::{Arc, Mutex, Once};
    use std::time::Duration;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid,
        HidD_GetPreparsedData, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    };

    use super::{Kind, PadState, SONY, kind, parse, path_ids};

    /// How often new pads are looked for.
    const RESCAN: Duration = Duration::from_secs(2);

    /// A pad being read: its device path, and its last state (`None` until
    /// its first report, and once it is gone).
    struct Pad {
        path: Vec<u16>,
        state: Arc<Mutex<Option<PadState>>>,
        alive: Arc<std::sync::atomic::AtomicBool>,
    }

    static PADS: Mutex<Vec<Pad>> = Mutex::new(Vec::new());

    /// The state of the `n`th PlayStation pad that is connected and has
    /// reported, in the order they were found.
    pub(in super::super) fn pad(n: usize) -> Option<PadState> {
        static START: Once = Once::new();
        START.call_once(|| {
            if let Err(e) = std::thread::Builder::new()
                .name("victory-sony-pads".into())
                .spawn(|| loop {
                    scan();
                    std::thread::sleep(RESCAN);
                })
            {
                tracing::warn!("PlayStation pads are not read: no thread ({e})");
            }
        });
        let pads = PADS.lock().unwrap_or_else(|e| e.into_inner());
        pads.iter()
            .filter(|p| p.alive.load(std::sync::atomic::Ordering::Relaxed))
            .filter_map(|p| *p.state.lock().unwrap_or_else(|e| e.into_inner()))
            .nth(n)
    }

    /// Looks for HID devices that are Sony pads and starts reading the new ones.
    fn scan() {
        PADS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|p| p.alive.load(std::sync::atomic::Ordering::Relaxed));
        // What the scan saw, said when it changes: run 13 had the owner's
        // PS5 pad plugged in and no line about it, and nothing to tell why.
        static SEEN: Mutex<Option<String>> = Mutex::new(None);
        let paths = hid_paths();
        let mut sonys = Vec::new();
        // Every device as vendor:product, for the line: run 14 found 20
        // and no Sony one, and could not say what the 20 were.
        let mut all = Vec::new();
        for path in paths.iter().cloned() {
            let shown = String::from_utf16_lossy(&path[..path.len().saturating_sub(1)]);
            let known = PADS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|p| p.path == path);
            if known {
                sonys.push("read already".to_owned());
                all.push(path_ids(&shown).map_or_else(|| "?".to_owned(), |(v, p)| format!("{v:04x}:{p:04x}")));
                continue;
            }
            let (vendor, product) = match ids(&path) {
                Ok(ids) => ids,
                Err(error) => {
                    // Not to be opened even without access: HidHide hides
                    // it from us, or it is going away. The path still says
                    // what it is.
                    let Some((vendor, product)) = path_ids(&shown) else {
                        all.push(format!("? (not opened: {error})"));
                        continue;
                    };
                    all.push(format!("{vendor:04x}:{product:04x} (not opened: {error})"));
                    if vendor == SONY {
                        let what = kind(product).map_or_else(|| "a Sony device".to_owned(), |k| format!("{k:?}"));
                        sonys.push(format!(
                            "{what} {product:#06x} HIDDEN FROM US ({error}: HidHide, from DS4Windows, hides it from every \
                             program not on its list; add victory.exe in the HidHide Configuration Client, or run DS4Windows, \
                             whose Xbox pad works here)"
                        ));
                    }
                    continue;
                }
            };
            all.push(format!("{vendor:04x}:{product:04x}"));
            if vendor != SONY {
                continue;
            }
            let Some(kind) = kind(product) else {
                sonys.push(format!("product {product:#06x} (not a pad we read)"));
                continue;
            };
            let Some((handle, length)) = open(&path) else {
                sonys.push(format!("{kind:?} {product:#06x} (could not be opened for reading: see the warning)"));
                continue;
            };
            sonys.push(format!("{kind:?} {product:#06x}"));
            let state = Arc::new(Mutex::new(None));
            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let (s, a) = (state.clone(), alive.clone());
            let handle = handle as usize;
            let started = std::thread::Builder::new()
                .name("victory-sony-pad".into())
                .spawn(move || read(handle as HANDLE, kind, length, &s, &a));
            if started.is_err() {
                // SAFETY: our handle, used by no thread.
                unsafe { CloseHandle(handle as HANDLE) };
                continue;
            }
            tracing::info!(
                "a PlayStation pad is read as a gamepad (VICTORY_SONY_PADS=0: not): {kind:?}, product {product:#06x}, reports of {length} bytes"
            );
            PADS.lock().unwrap_or_else(|e| e.into_inner()).push(Pad { path, state, alive });
        }
        let seen = format!(
            "{} HID devices, Sony ones: [{}]; all of them (vendor:product, Sony is 054c): {}",
            paths.len(),
            sonys.join(", "),
            all.join(", ")
        );
        let mut last = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() != Some(seen.as_str()) {
            tracing::info!(
                "PlayStation pads: the scan found {seen} (no Sony one: the pad is off or not plugged in, or not a HID device here)"
            );
            *last = Some(seen);
        }
    }

    /// Reads reports until the pad is gone.
    fn read(
        handle: HANDLE,
        kind: Kind,
        length: usize,
        state: &Mutex<Option<PadState>>,
        alive: &std::sync::atomic::AtomicBool,
    ) {
        let mut report = vec![0u8; length.max(64)];
        let mut first = true;
        loop {
            let mut got = 0u32;
            // SAFETY: room for `report.len()` bytes; a handle opened for
            // reading, synchronously.
            let ok = unsafe {
                ReadFile(handle, report.as_mut_ptr(), report.len() as u32, &mut got, std::ptr::null_mut())
            } != 0;
            if !ok {
                break;
            }
            let pad = parse(kind, &report[..got as usize]);
            if first {
                first = false;
                tracing::info!(
                    id = format_args!("{:#04x}", report[0]),
                    bytes = got,
                    read = pad.is_some(),
                    "a PlayStation pad's first report"
                );
            }
            if let Some(pad) = pad {
                *state.lock().unwrap_or_else(|e| e.into_inner()) = Some(pad);
            }
        }
        alive.store(false, std::sync::atomic::Ordering::Relaxed);
        *state.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // SAFETY: our handle; this thread was its only user.
        unsafe { CloseHandle(handle) };
        tracing::info!("a PlayStation pad is gone ({kind:?})");
    }

    /// The device paths (NUL-terminated) of the HID devices present.
    fn hid_paths() -> Vec<Vec<u16>> {
        let mut guid = windows_sys::core::GUID::from_u128(0);
        // SAFETY: room for the GUID.
        unsafe { HidD_GetHidGuid(&mut guid) };
        // SAFETY: plain query.
        let set = unsafe {
            SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
        };
        if set == INVALID_HANDLE_VALUE as isize || set == 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "PlayStation pads: the HID devices could not be listed");
            return Vec::new();
        }
        let mut paths = Vec::new();
        for i in 0.. {
            let mut data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: a live set; room for the data.
            if unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, i, &mut data) } == 0 {
                break;
            }
            let mut size = 0u32;
            // SAFETY: asks for the size only.
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &data, std::ptr::null_mut(), 0, &mut size, std::ptr::null_mut())
            };
            if (size as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
                continue;
            }
            // u32s: aligned for the header.
            let mut buffer = vec![0u32; (size as usize).div_ceil(4)];
            let detail = buffer.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            // SAFETY: the buffer holds `size` bytes, aligned for the header.
            unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
            // SAFETY: as above.
            let ok = unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &data, detail, size, std::ptr::null_mut(), std::ptr::null_mut())
            } != 0;
            if !ok {
                continue;
            }
            // SAFETY: the path follows the size field, NUL-terminated, inside the buffer.
            let path = unsafe {
                let start = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let most = (size as usize - 4) / 2;
                let len = (0..most).take_while(|&k| *start.add(k) != 0).count();
                let mut path = std::slice::from_raw_parts(start, len).to_vec();
                path.push(0);
                path
            };
            paths.push(path);
        }
        // SAFETY: the set made above.
        unsafe { SetupDiDestroyDeviceInfoList(set) };
        paths
    }

    /// The vendor and product numbers of the HID device at `path`; the
    /// error if it cannot be opened even without access.
    fn ids(path: &[u16]) -> Result<(u16, u16), std::io::Error> {
        // No access asked: enough for the attributes, and it opens devices
        // others hold (keyboards, mice).
        // SAFETY: a NUL-terminated path.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            VendorID: 0,
            ProductID: 0,
            VersionNumber: 0,
        };
        // SAFETY: an open device; room for the attributes.
        let ok = unsafe { HidD_GetAttributes(handle, &mut attributes) };
        // SAFETY: ours.
        unsafe { CloseHandle(handle) };
        if ok {
            Ok((attributes.VendorID, attributes.ProductID))
        } else {
            Err(std::io::Error::other("the attributes were not given"))
        }
    }

    /// The pad opened for reading, and the length of its input reports.
    fn open(path: &[u16]) -> Option<(HANDLE, usize)> {
        // SAFETY: a NUL-terminated path.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "a PlayStation pad could not be opened for reading (another program, such as Steam or DS4Windows, may hold it alone)"
            );
            return None;
        }
        let mut data = 0;
        // SAFETY: plain numbers; all zeros is a valid value.
        let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
        // SAFETY: an open device; the parsed data is given back below.
        let length = unsafe {
            if HidD_GetPreparsedData(handle, &mut data) {
                HidP_GetCaps(data, &mut caps);
                HidD_FreePreparsedData(data);
                usize::from(caps.InputReportByteLength)
            } else {
                0
            }
        };
        if length == 0 {
            // SAFETY: ours.
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some((handle, length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(len: usize, id: u8, at: usize, bytes: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; len];
        r[0] = id;
        r[at..at + bytes.len()].copy_from_slice(bytes);
        r
    }

    #[test]
    fn a_dualsense_over_usb_reads_its_full_report() {
        // Sticks, L2 255, R2 0, counter, cross + hat right, options + R1, PS.
        let r = report(64, 0x01, 1, &[255, 0, 128, 128, 255, 0, 7, 0x22, 0x22, 0x01]);
        let pad = parse(Kind::DualSense, &r).unwrap();
        assert_eq!(pad.buttons, A | RIGHT | MENU | RIGHT_SHOULDER);
        assert_eq!((pad.left_x, pad.left_y), (1.0, 1.0));
        assert_eq!((pad.right_x, pad.right_y), (0.0, 0.0));
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
    }

    #[test]
    fn a_dualsense_over_bluetooth_reads_both_reports() {
        // The short report: buttons before the triggers. Hat centred.
        let r = report(78, 0x01, 1, &[128, 255, 128, 128, 0x18, 0x10, 0, 0, 255]);
        let pad = parse(Kind::DualSense, &r).unwrap();
        assert_eq!(pad.buttons, X | VIEW);
        assert_eq!(pad.left_y, -1.0);
        assert_eq!(pad.right_trigger, 1.0);
        // The full one, a byte later.
        let r = report(78, 0x31, 2, &[128, 128, 0, 128, 0, 255, 0, 0x44, 0x40, 0]);
        let pad = parse(Kind::DualSense, &r).unwrap();
        assert_eq!(pad.buttons, B | DOWN | LEFT_STICK);
        assert_eq!(pad.right_x, -1.0);
        assert_eq!(pad.right_trigger, 1.0);
    }

    #[test]
    fn a_dualshock_4_reads_usb_and_bluetooth() {
        let r = report(64, 0x01, 1, &[128, 128, 128, 128, 0x88, 0x01, 0, 255, 0]);
        let pad = parse(Kind::DualShock4, &r).unwrap();
        assert_eq!(pad.buttons, Y | LEFT_SHOULDER);
        assert_eq!(pad.left_trigger, 1.0);
        let r = report(78, 0x11, 3, &[128, 128, 128, 128, 0x06, 0x80, 0, 0, 0]);
        assert_eq!(parse(Kind::DualShock4, &r).unwrap().buttons, LEFT | RIGHT_STICK);
    }

    #[test]
    fn a_device_path_names_its_vendor_and_product() {
        assert_eq!(
            path_ids(r"\\?\hid#vid_054c&pid_0ce6&mi_03#8&2b1a3c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"),
            Some((0x054C, 0x0CE6))
        );
        assert_eq!(
            path_ids(r"\\?\HID#{00001124-0000-1000-8000-00805F9B34FB}_VID&0002054C_PID&0DF2#9&1f0e&0&0000#{4d1e55b2}"),
            Some((0x054C, 0x0DF2))
        );
        assert_eq!(path_ids(r"\\?\hid#vid_046d&pid_c52b&mi_02&col01#a"), Some((0x046D, 0xC52B)));
        assert_eq!(path_ids(r"\\?\hid#acpi0c50#1"), None);
    }

    #[test]
    fn other_reports_and_pads_are_not_read() {
        assert_eq!(parse(Kind::DualSense, &[0x05, 0, 0]), None);
        assert_eq!(parse(Kind::DualSense, &[0x01, 0, 0]), None);
        assert_eq!(kind(0x1234), None);
        assert_eq!(kind(0x0CE6), Some(Kind::DualSense));
    }
}
