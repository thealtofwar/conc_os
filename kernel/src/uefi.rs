//! Minimal UEFI FFI layer.
//!
//! We only need a handful of things from firmware before we take over the
//! machine: the memory map, the framebuffer (if any), the ACPI root pointer
//! and `ExitBootServices`.  Rather than pulling in a full UEFI crate, the
//! handful of tables we touch are defined here by hand.

#![allow(dead_code)]

use core::ffi::c_void;

pub type Handle = *mut c_void;
pub type Status = usize;

pub const SUCCESS: Status = 0;
const ERR: usize = 1 << 63;
pub const INVALID_PARAMETER: Status = ERR | 2;
pub const BUFFER_TOO_SMALL: Status = ERR | 5;
pub const NOT_FOUND: Status = ERR | 14;

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

pub const GOP_GUID: Guid = Guid(0x9042a9de, 0x23dc, 0x4a38, [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]);
pub const ACPI2_TABLE_GUID: Guid = Guid(0x8868e871, 0xe4f1, 0x11d3, [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81]);
pub const ACPI1_TABLE_GUID: Guid = Guid(0xeb9d2d30, 0x2d88, 0x11d3, [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d]);

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut c_void,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

#[repr(C)]
pub struct SimpleTextOutput {
    pub reset: usize,
    pub output_string: unsafe extern "efiapi" fn(*mut SimpleTextOutput, *const u16) -> Status,
    pub test_string: usize,
    pub query_mode: usize,
    pub set_mode: usize,
    pub set_attribute: usize,
    pub clear_screen: unsafe extern "efiapi" fn(*mut SimpleTextOutput) -> Status,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(u64, usize) -> Status,
    pub get_memory_map:
        unsafe extern "efiapi" fn(*mut usize, *mut MemoryDescriptor, *mut usize, *mut usize, *mut u32) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(*mut u8) -> Status,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: usize,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: usize,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    pub get_next_monotonic_count: usize,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: usize,
    pub locate_protocol: unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> Status,
}

/// UEFI memory descriptor.  Firmware may use a larger stride than
/// `size_of::<MemoryDescriptor>()`; always iterate with the reported size.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryDescriptor {
    pub ty: u32,
    pub pad: u32,
    pub phys_start: u64,
    pub virt_start: u64,
    pub pages: u64,
    pub attr: u64,
}

pub mod mem_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
}

impl MemoryDescriptor {
    pub fn end(&self) -> u64 {
        self.phys_start + self.pages * 4096
    }
    /// Memory we may hand to the frame allocator once firmware is gone.
    pub fn is_free_after_boot(&self) -> bool {
        matches!(
            self.ty,
            mem_type::CONVENTIONAL | mem_type::BOOT_SERVICES_CODE | mem_type::BOOT_SERVICES_DATA
        )
    }
    /// Memory that is backed by RAM (as opposed to device MMIO / holes).
    pub fn is_ram(&self) -> bool {
        matches!(
            self.ty,
            mem_type::LOADER_CODE
                | mem_type::LOADER_DATA
                | mem_type::BOOT_SERVICES_CODE
                | mem_type::BOOT_SERVICES_DATA
                | mem_type::RUNTIME_SERVICES_CODE
                | mem_type::RUNTIME_SERVICES_DATA
                | mem_type::CONVENTIONAL
                | mem_type::ACPI_RECLAIM
                | mem_type::ACPI_NVS
        )
    }
}

#[repr(C)]
pub struct GraphicsOutput {
    pub query_mode: usize,
    pub set_mode: usize,
    pub blt: usize,
    pub mode: *const GopMode,
}

#[repr(C)]
pub struct GopMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const GopModeInfo,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct GopModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
    pub pixels_per_scan_line: u32,
}

/// Framebuffer description captured from GOP before boot services go away.
#[derive(Clone, Copy, Debug, Default)]
pub struct Framebuffer {
    pub base: u64,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// 0 = RGBX8888, 1 = BGRX8888, 2 = bitmask, 3 = blt-only.
    pub pixel_format: u32,
}

pub const MAX_MEMMAP_ENTRIES: usize = 512;

/// Everything the kernel needs from firmware, copied out before
/// `ExitBootServices` so it lives in our own image rather than in
/// boot-services memory.
pub struct BootInfo {
    pub memmap: [MemoryDescriptor; MAX_MEMMAP_ENTRIES],
    pub memmap_len: usize,
    pub framebuffer: Option<Framebuffer>,
    pub rsdp: u64,
}

impl BootInfo {
    pub const fn empty() -> Self {
        BootInfo {
            memmap: [MemoryDescriptor { ty: 0, pad: 0, phys_start: 0, virt_start: 0, pages: 0, attr: 0 };
                MAX_MEMMAP_ENTRIES],
            memmap_len: 0,
            framebuffer: None,
            rsdp: 0,
        }
    }
    pub fn descriptors(&self) -> &[MemoryDescriptor] {
        &self.memmap[..self.memmap_len]
    }
}

/// Write a message on the firmware console (visible in the QEMU window before
/// we have our own framebuffer console).
pub unsafe fn con_out(st: *mut SystemTable, msg: &str) {
    let con = (*st).con_out;
    if con.is_null() {
        return;
    }
    let mut buf = [0u16; 128];
    let mut i = 0;
    for b in msg.bytes() {
        if i >= buf.len() - 3 {
            break;
        }
        if b == b'\n' {
            buf[i] = b'\r' as u16;
            i += 1;
        }
        buf[i] = b as u16;
        i += 1;
    }
    buf[i] = 0;
    ((*con).output_string)(con, buf.as_ptr());
}

unsafe fn locate_gop(bs: *mut BootServices) -> Option<Framebuffer> {
    let mut iface: *mut c_void = core::ptr::null_mut();
    let status = ((*bs).locate_protocol)(&GOP_GUID, core::ptr::null_mut(), &mut iface);
    if status != SUCCESS || iface.is_null() {
        return None;
    }
    let gop = iface as *mut GraphicsOutput;
    let mode = (*gop).mode;
    if mode.is_null() || (*mode).info.is_null() {
        return None;
    }
    let info = &*(*mode).info;
    Some(Framebuffer {
        base: (*mode).frame_buffer_base,
        size: (*mode).frame_buffer_size,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        pixel_format: info.pixel_format,
    })
}

unsafe fn find_rsdp(st: *mut SystemTable) -> u64 {
    let n = (*st).number_of_table_entries;
    let tables = (*st).configuration_table;
    let mut rsdp1 = 0u64;
    for i in 0..n {
        let t = &*tables.add(i);
        if t.vendor_guid == ACPI2_TABLE_GUID {
            return t.vendor_table as u64;
        }
        if t.vendor_guid == ACPI1_TABLE_GUID {
            rsdp1 = t.vendor_table as u64;
        }
    }
    rsdp1
}

/// Collect boot information and leave the firmware environment.  On return
/// the firmware is gone: no more boot services, no firmware timer interrupts,
/// and the memory map in `info` is authoritative.
pub unsafe fn exit_boot_services(image: Handle, st: *mut SystemTable, info: &mut BootInfo) -> Result<(), Status> {
    let bs = (*st).boot_services;

    // Disable the firmware watchdog: by default it would reset the machine
    // after five minutes.
    ((*bs).set_watchdog_timer)(0, 0, 0, core::ptr::null());

    info.framebuffer = locate_gop(bs);
    info.rsdp = find_rsdp(st);

    // Pool buffer for the memory map.  Allocate it *before* the final
    // GetMemoryMap so the map does not change under us.
    let buf_size: usize = 64 * 1024;
    let mut buf: *mut u8 = core::ptr::null_mut();
    let s = ((*bs).allocate_pool)(mem_type::LOADER_DATA, buf_size, &mut buf);
    if s != SUCCESS {
        return Err(s);
    }

    let mut key: usize = 0;
    let mut desc_size: usize = 0;
    let mut desc_ver: u32 = 0;
    let mut size;
    loop {
        size = buf_size;
        let s = ((*bs).get_memory_map)(
            &mut size,
            buf as *mut MemoryDescriptor,
            &mut key,
            &mut desc_size,
            &mut desc_ver,
        );
        if s != SUCCESS {
            return Err(s);
        }
        let s = ((*bs).exit_boot_services)(image, key);
        if s == SUCCESS {
            break;
        }
        if s != INVALID_PARAMETER {
            return Err(s);
        }
        // Map changed between the two calls; try again.
    }

    // Copy the map into our own static storage.
    let count = size / desc_size;
    let mut n = 0;
    for i in 0..count {
        if n >= MAX_MEMMAP_ENTRIES {
            break;
        }
        let d = &*(buf.add(i * desc_size) as *const MemoryDescriptor);
        if d.pages == 0 {
            continue;
        }
        info.memmap[n] = *d;
        n += 1;
    }
    info.memmap_len = n;
    Ok(())
}
