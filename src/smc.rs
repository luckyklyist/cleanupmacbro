//! Minimal Apple SMC client (the chip that controls charging), via IOKit.
//! Reading works for any user; writing needs root, so only the charge helper writes.

use std::ffi::{c_char, c_void};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyData {
    key: u32,
    vers: Vers,
    p_limit: PLimit,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

const KERNEL_INDEX_SMC: u32 = 2;
const CMD_READ_BYTES: u8 = 5;
const CMD_WRITE_BYTES: u8 = 6;
const CMD_READ_KEYINFO: u8 = 9;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOConnectCallStructMethod(
        connection: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
}

extern "C" {
    static mach_task_self_: u32;
}

pub struct Smc(u32);

impl Smc {
    pub fn open() -> Option<Smc> {
        unsafe {
            let service = IOServiceGetMatchingService(0, IOServiceMatching(c"AppleSMC".as_ptr()));
            if service == 0 {
                return None;
            }
            let mut conn = 0;
            let r = IOServiceOpen(service, mach_task_self_, 0, &mut conn);
            IOObjectRelease(service);
            (r == 0).then_some(Smc(conn))
        }
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut size = std::mem::size_of::<KeyData>();
        let r = unsafe {
            IOConnectCallStructMethod(
                self.0,
                KERNEL_INDEX_SMC,
                input as *const _ as *const c_void,
                std::mem::size_of::<KeyData>(),
                &mut out as *mut _ as *mut c_void,
                &mut size,
            )
        };
        (r == 0 && out.result == 0).then_some(out)
    }

    fn info(&self, key: &str) -> Option<KeyInfo> {
        let input = KeyData { key: fourcc(key), data8: CMD_READ_KEYINFO, ..Default::default() };
        self.call(&input).map(|o| o.key_info)
    }

    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        let info = self.info(key)?;
        let input = KeyData { key: fourcc(key), key_info: info, data8: CMD_READ_BYTES, ..Default::default() };
        let out = self.call(&input)?;
        Some(out.bytes[..(info.data_size as usize).min(32)].to_vec())
    }

    pub fn write(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let info = self.info(key).ok_or(format!("SMC key {key} not found"))?;
        if info.data_size as usize != bytes.len() {
            return Err(format!("SMC key {key} expects {} bytes", info.data_size));
        }
        let mut input = KeyData { key: fourcc(key), key_info: info, data8: CMD_WRITE_BYTES, ..Default::default() };
        input.bytes[..bytes.len()].copy_from_slice(bytes);
        self.call(&input).map(|_| ()).ok_or(format!("Writing {key} failed (needs root)"))
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.0);
        }
    }
}

fn fourcc(key: &str) -> u32 {
    key.bytes().take(4).fold(0, |acc, b| (acc << 8) | b as u32)
}

/// Which charge-inhibit method this Mac's firmware supports.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChargeKey {
    /// Newer Apple Silicon firmware: CHTE (4 bytes, 1 = don't charge).
    Chte,
    /// Older firmware / Intel: CH0B + CH0C (1 byte, 2 = don't charge).
    Ch0b,
}

pub fn charge_key(smc: &Smc) -> Option<ChargeKey> {
    if smc.read("CHTE").is_some() {
        Some(ChargeKey::Chte)
    } else if smc.read("CH0B").is_some() {
        Some(ChargeKey::Ch0b)
    } else {
        None
    }
}

/// Is charging currently allowed by the SMC?
pub fn charging_allowed(smc: &Smc) -> Option<bool> {
    charge_key(smc)?;
    let blocked = |k: &str| smc.read(k).map_or(false, |b| b.iter().any(|x| *x != 0));
    Some(!blocked("CHTE") && !blocked("CH0B"))
}

/// Writes every charge key this firmware has (CHTE and/or CH0B+CH0C); succeeds if any write works.
pub fn set_charging(smc: &Smc, allow: bool) -> Result<(), String> {
    let mut ok = false;
    let mut last = String::from("This Mac doesn't expose charge control");
    if smc.read("CHTE").is_some() {
        match smc.write("CHTE", if allow { &[0, 0, 0, 0] } else { &[1, 0, 0, 0] }) {
            Ok(()) => ok = true,
            Err(e) => last = e,
        }
    }
    if smc.read("CH0B").is_some() {
        let v = if allow { 0 } else { 2 };
        match smc.write("CH0B", &[v]).and_then(|_| smc.write("CH0C", &[v])) {
            Ok(()) => ok = true,
            Err(e) => last = e,
        }
    }
    if ok { Ok(()) } else { Err(last) }
}

/// Turns the power adapter off/on for the battery. With the adapter "off" the Mac runs on
/// battery while still plugged in (used to discharge down to the limit).
pub fn set_adapter(smc: &Smc, enabled: bool) -> Result<(), String> {
    let mut ok = false;
    let mut last = String::from("This Mac can't discharge while plugged in");
    if smc.read("CHIE").is_some() {
        match smc.write("CHIE", &[if enabled { 0x00 } else { 0x08 }]) {
            Ok(()) => ok = true,
            Err(e) => last = e,
        }
    }
    if !ok && smc.read("CH0I").is_some() {
        match smc.write("CH0I", &[if enabled { 0x00 } else { 0x01 }]) {
            Ok(()) => ok = true,
            Err(e) => last = e,
        }
    }
    if ok { Ok(()) } else { Err(last) }
}

pub fn adapter_enabled(smc: &Smc) -> bool {
    let off = |k: &str| smc.read(k).map_or(false, |b| b.iter().any(|x| *x != 0));
    !off("CHIE") && !off("CH0I")
}

#[derive(Clone, Copy, PartialEq)]
pub enum Led {
    System,
    Green,
    Orange,
}

/// MagSafe connector light.
pub fn set_led(smc: &Smc, led: Led) -> Result<(), String> {
    let v = match led {
        Led::System => 0x00,
        Led::Green => 0x03,
        Led::Orange => 0x04,
    };
    smc.write("ACLC", &[v])
}
