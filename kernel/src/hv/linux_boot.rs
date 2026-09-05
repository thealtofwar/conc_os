//! Linux boot: build a template from a kernel image on disk using the
//! x86 64-bit boot protocol, Firecracker-style.
//!
//! Layout of the low megabyte we hand to the kernel:
//!
//! ```text
//! 0x0500  GDT (null, code, data, TSS)      0x7000  boot_params ("zero page")
//! 0x0520  IDT (8 zero bytes)               0x8FF0  initial stack pointer
//! 0x9000  PML4  0xA000 PDPT  0xB000.. PDs  0x20000 command line
//! ```
//!
//! The kernel itself is loaded at its physical addresses (vmlinux ELF) or at
//! its preferred address (bzImage, entered at +0x200), the initramfs sits at
//! the top of guest RAM, and everything is described to Linux through the
//! e820 map and setup_header fields in the zero page.

#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::image::{BootState, Template, TemplateKind};
use super::svm::seg_attr;
use crate::arch::cpu::{cr0, cr4, efer};
use crate::disk::images::{self, ImageEntry};
use crate::mm::PAGE_SIZE;
use crate::virtio::blk::VirtioBlk;

pub const GDT_ADDR: u64 = 0x500;
pub const IDT_ADDR: u64 = 0x520;
pub const ZERO_PAGE: u64 = 0x7000;
pub const BOOT_STACK: u64 = 0x8FF0;
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xA000;
pub const PD_ADDR: u64 = 0xB000;
pub const CMDLINE_ADDR: u64 = 0x20000;
pub const CMDLINE_MAX: usize = 4096;
pub const MIN_MEM: u64 = 32 << 20;
pub const MAX_MEM: u64 = 2 << 30;
pub const DEFAULT_MEM: u64 = 128 << 20;
/// Highest address usable for RAM below the 32-bit MMIO hole.
const LOW_RAM_LIMIT: u64 = 0xD000_0000;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// A byte range of the image file to copy to a guest physical address.
#[derive(Clone, Copy, Debug)]
struct Copy {
    file_off: u64,
    len: u64,
    gpa: u64,
}

struct Plan {
    copies: Vec<Copy>,
    entry: u64,
    kernel_end: u64,
    /// For bzImage: the setup header bytes to seed boot_params with.
    setup_header: Option<Vec<u8>>,
    format: &'static str,
    /// Guest pages of the first (text) segment, for sharing statistics.
    text_pages: Option<(u32, u32)>,
}

fn plan_elf(head: &[u8], file_size: u64, mem_size: u64) -> Result<Plan, String> {
    if head.len() < 64 || &head[0..4] != b"\x7fELF" || head[4] != 2 || u16_at(head, 18) != 0x3E {
        return Err("not an x86_64 ELF64 kernel".into());
    }
    let entry = u64_at(head, 24);
    let phoff = u64_at(head, 32) as usize;
    let phentsize = u16_at(head, 54) as usize;
    let phnum = u16_at(head, 56) as usize;
    if phentsize < 56 || phoff + phnum * phentsize > head.len() {
        return Err("program headers beyond the first 64 KiB are not supported".into());
    }
    let mut copies = Vec::new();
    let mut kernel_end = 0u64;
    let mut text_pages = None;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if u32_at(head, p) != 1 {
            continue;
        }
        let offset = u64_at(head, p + 8);
        let paddr = u64_at(head, p + 24);
        let filesz = u64_at(head, p + 32);
        let memsz = u64_at(head, p + 40);
        if filesz == 0 && memsz == 0 {
            continue;
        }
        if offset + filesz > file_size {
            return Err("ELF segment beyond end of file".into());
        }
        if paddr < 0x100000 || paddr + memsz > mem_size {
            return Err(format!(
                "kernel segment {:#x}+{:#x} does not fit in {} MiB of guest memory",
                paddr,
                memsz,
                mem_size >> 20
            ));
        }
        if filesz > 0 {
            if text_pages.is_none() {
                // The first PT_LOAD of a vmlinux is .text/.rodata.
                text_pages = Some(((paddr / 4096) as u32, ((paddr + filesz + 4095) / 4096) as u32));
            }
            copies.push(Copy { file_off: offset, len: filesz, gpa: paddr });
        }
        kernel_end = kernel_end.max(paddr + memsz);
    }
    if copies.is_empty() {
        return Err("ELF kernel has no loadable segments".into());
    }
    if entry >= mem_size {
        return Err(format!("ELF entry {:#x} outside guest memory", entry));
    }
    Ok(Plan { copies, entry, kernel_end, setup_header: None, format: "vmlinux (ELF)", text_pages })
}

fn plan_bzimage(head: &[u8], file_size: u64, mem_size: u64) -> Result<Plan, String> {
    if head.len() < 0x270 || u16_at(head, 0x1FE) != 0xAA55 || u32_at(head, 0x202) != 0x5372_6448 {
        return Err("not a bzImage (missing boot_flag/HdrS)".into());
    }
    let version = u16_at(head, 0x206);
    if version < 0x020C {
        return Err(format!("bzImage boot protocol {:#x} too old for 64-bit entry (need >= 2.12)", version));
    }
    let xloadflags = u16_at(head, 0x236);
    if xloadflags & 1 == 0 {
        return Err("bzImage lacks XLF_KERNEL_64 (no 64-bit entry point)".into());
    }
    let mut setup_sects = head[0x1F1] as u64;
    if setup_sects == 0 {
        setup_sects = 4;
    }
    let pm_offset = (setup_sects + 1) * 512;
    if pm_offset >= file_size {
        return Err("bzImage protected-mode part missing".into());
    }
    let relocatable = head[0x234] != 0;
    let pref = u64_at(head, 0x258);
    let init_size = u32_at(head, 0x260) as u64;
    let load_addr = if relocatable && pref != 0 { pref } else { 0x100000 };
    let pm_len = file_size - pm_offset;
    let kernel_end = load_addr + init_size.max(pm_len);
    if kernel_end > mem_size {
        return Err(format!("bzImage needs {} MiB at {:#x}", init_size >> 20, load_addr));
    }
    let hdr_end = 0x202 + head[0x201] as usize;
    let setup_header = head[0x1F1..hdr_end.min(head.len())].to_vec();
    Ok(Plan {
        copies: alloc::vec![Copy { file_off: pm_offset, len: pm_len, gpa: load_addr }],
        entry: load_addr + 0x200,
        kernel_end,
        setup_header: Some(setup_header),
        format: "bzImage",
        text_pages: None,
    })
}

fn boot_state(entry: u64) -> BootState {
    BootState {
        rip: entry,
        rsp: BOOT_STACK,
        rdi: 0,
        rsi: ZERO_PAGE,
        rdx: 0,
        cr0: cr0::PE | cr0::MP | cr0::ET | cr0::NE | cr0::WP | cr0::PG,
        cr3: PML4_ADDR,
        cr4: cr4::PAE,
        efer: efer::LME | efer::LMA,
        gdtr_base: GDT_ADDR,
        gdtr_limit: 0x1F,
        idtr_base: IDT_ADDR,
        idtr_limit: 7,
        cs: 0x08,
        ds: 0x10,
        tr: 0x18,
        tr_attr: seg_attr::TSS64,
        tr_limit: 0xFFFF_FFFF,
    }
}

pub const MP_FLOAT_ADDR: u64 = 0xF0000;
pub const MP_TABLE_ADDR: u64 = 0xF0010;

fn checksum_fix(v: &mut [u8], at: usize) {
    v[at] = 0;
    let sum = v.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    v[at] = (!sum).wrapping_add(1);
}

/// Write the MP floating pointer and configuration table (Intel MPS 1.4)
/// into the BIOS area the kernel scans.
fn write_mp_table(t: &mut Template) -> Result<(), &'static str> {
    use super::devices::ioapic::{IOAPIC_BASE, IOAPIC_ID};
    let id = crate::arch::cpu::cpuid(1, 0);
    let mut c: Vec<u8> = Vec::with_capacity(256);
    c.extend_from_slice(b"PCMP");
    c.extend_from_slice(&0u16.to_le_bytes()); // length, fixed below
    c.push(4); // spec revision 1.4
    c.push(0); // checksum, fixed below
    c.extend_from_slice(b"CONC_OS ");
    c.extend_from_slice(b"MICROVM     ");
    c.extend_from_slice(&0u32.to_le_bytes()); // oem table
    c.extend_from_slice(&0u16.to_le_bytes()); // oem table size
    c.extend_from_slice(&0u16.to_le_bytes()); // entry count, fixed below
    c.extend_from_slice(&0xFEE0_0000u32.to_le_bytes()); // local APIC
    c.extend_from_slice(&0u16.to_le_bytes()); // extended table length
    c.push(0); // extended checksum
    c.push(0);
    let mut count = 0u16;
    // Processor: enabled, bootstrap.
    c.push(0);
    c.push(0); // local APIC id
    c.push(0x14); // local APIC version
    c.push(0x03);
    c.extend_from_slice(&id.eax.to_le_bytes());
    c.extend_from_slice(&id.edx.to_le_bytes());
    c.extend_from_slice(&[0u8; 8]);
    count += 1;
    // Bus 0: ISA.
    c.push(1);
    c.push(0);
    c.extend_from_slice(b"ISA   ");
    count += 1;
    // I/O APIC.
    c.push(2);
    c.push(IOAPIC_ID);
    c.push(0x20);
    c.push(1);
    c.extend_from_slice(&(IOAPIC_BASE as u32).to_le_bytes());
    count += 1;
    // ExtINT from the 8259 on pin 0, then the ISA IRQs (IRQ 0 -> pin 2).
    let mut io_int = |int_type: u8, irq: u8, pin: u8| {
        c.push(3);
        c.push(int_type);
        c.extend_from_slice(&0u16.to_le_bytes()); // polarity/trigger: bus default
        c.push(0);
        c.push(irq);
        c.push(IOAPIC_ID);
        c.push(pin);
    };
    io_int(3, 0, 0);
    count += 1;
    for irq in 0..16u8 {
        if irq == 2 {
            continue;
        }
        io_int(0, irq, if irq == 0 { 2 } else { irq });
        count += 1;
    }
    // Local interrupts: ExtINT on LINT0, NMI on LINT1, all processors.
    for (int_type, lint) in [(3u8, 0u8), (1, 1)] {
        c.push(4);
        c.push(int_type);
        c.extend_from_slice(&0u16.to_le_bytes());
        c.push(0);
        c.push(0);
        c.push(0xFF);
        c.push(lint);
        count += 1;
    }
    let len = c.len() as u16;
    c[4..6].copy_from_slice(&len.to_le_bytes());
    c[34..36].copy_from_slice(&count.to_le_bytes());
    checksum_fix(&mut c, 7);

    let mut f: Vec<u8> = Vec::with_capacity(16);
    f.extend_from_slice(b"_MP_");
    f.extend_from_slice(&(MP_TABLE_ADDR as u32).to_le_bytes());
    f.push(1); // length in 16-byte units
    f.push(4); // spec revision
    f.push(0); // checksum
    f.extend_from_slice(&[0u8; 5]); // feature bytes: config table present, no IMCR
    checksum_fix(&mut f, 10);

    t.write(MP_FLOAT_ADDR, &f)?;
    t.write(MP_TABLE_ADDR, &c)?;
    Ok(())
}

/// Stream `entry` from disk, copying the planned ranges into the template.
async fn copy_image(dev: &Arc<VirtioBlk>, entry: &ImageEntry, copies: &[Copy], t: &mut Template) -> Result<(), String> {
    let mut err: Option<String> = None;
    let mut sink = |off: u64, bytes: &[u8]| {
        if err.is_some() {
            return;
        }
        let chunk_end = off + bytes.len() as u64;
        for c in copies {
            let c_end = c.file_off + c.len;
            let lo = off.max(c.file_off);
            let hi = chunk_end.min(c_end);
            if lo >= hi {
                continue;
            }
            let src = &bytes[(lo - off) as usize..(hi - off) as usize];
            if let Err(e) = t.write(c.gpa + (lo - c.file_off), src) {
                err = Some(String::from(e));
                return;
            }
        }
    };
    images::read_image(dev, entry, &mut sink).await.map_err(|e| format!("disk read failed: {:?}", e))?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Build a Linux template: `mem_size` bytes of guest RAM, kernel from
/// `kernel`, optional initramfs, and the given command line.
pub async fn build_template(
    dev: &Arc<VirtioBlk>,
    kernel: &ImageEntry,
    initrd: Option<&ImageEntry>,
    cmdline: &str,
    mem_size: u64,
) -> Result<Arc<Template>, String> {
    if mem_size < MIN_MEM || mem_size > MAX_MEM || mem_size % (2 << 20) != 0 {
        return Err("linux memory size must be 32 MiB .. 2 GiB, a multiple of 2 MiB".into());
    }
    if cmdline.len() >= CMDLINE_MAX {
        return Err("command line too long".into());
    }

    // Header: first 64 KiB of the kernel image.
    let head_len = kernel.size.min(64 * 1024);
    let head_entry = ImageEntry { name: kernel.name.clone(), kind: kernel.kind, offset: kernel.offset, size: head_len };
    let head = images::read_image_vec(dev, &head_entry).await.map_err(|e| format!("disk read failed: {:?}", e))?;
    let plan = if head.len() >= 4 && &head[0..4] == b"\x7fELF" {
        plan_elf(&head, kernel.size, mem_size)?
    } else {
        plan_bzimage(&head, kernel.size, mem_size)?
    };

    // The template is named after the image set it came from, so `linux
    // snapshots` and `vm info` can say which kernel a VM is running.
    let mut t = Template::new_empty(TemplateKind::Linux, &kernel.name, mem_size, boot_state(plan.entry));
    t.image = Some(kernel.name.clone());

    // Kernel.
    copy_image(dev, kernel, &plan.copies, &mut t).await?;
    t.image_bytes = plan.copies.iter().map(|c| c.len as usize).sum();
    t.text_pages = plan.text_pages;

    // Initramfs at the top of low RAM.
    let mut initrd_gpa = 0u64;
    let mut initrd_size = 0u64;
    if let Some(rd) = initrd {
        let top = mem_size.min(LOW_RAM_LIMIT);
        let gpa = (top - rd.size) & !(PAGE_SIZE as u64 - 1);
        if gpa < plan.kernel_end + (1 << 20) {
            return Err(format!(
                "initramfs ({} KiB) does not fit above the kernel (ends {:#x}) in {} MiB",
                rd.size >> 10,
                plan.kernel_end,
                mem_size >> 20
            ));
        }
        copy_image(dev, rd, &[Copy { file_off: 0, len: rd.size, gpa }], &mut t).await?;
        initrd_gpa = gpa;
        initrd_size = rd.size;
        t.image_bytes += rd.size as usize;
    }

    // Page tables: identity map [0, mem_size) with 2 MiB pages, A/D pre-set.
    const AD: u64 = 0x60;
    t.write_u64(PML4_ADDR, PDPT_ADDR | 0x3 | AD).map_err(String::from)?;
    let gib = (mem_size + (1 << 30) - 1) >> 30;
    for g in 0..gib {
        let pd = PD_ADDR + g * 0x1000;
        t.write_u64(PDPT_ADDR + g * 8, pd | 0x3 | AD).map_err(String::from)?;
        for i in 0..512u64 {
            let pa = (g << 30) | (i << 21);
            if pa >= mem_size {
                break;
            }
            t.write_u64(pd + i * 8, pa | 0x83 | AD).map_err(String::from)?;
        }
    }

    // GDT and empty IDT.
    t.write_u64(GDT_ADDR, 0).map_err(String::from)?;
    t.write_u64(GDT_ADDR + 8, 0x00AF_9B00_0000_FFFF).map_err(String::from)?;
    t.write_u64(GDT_ADDR + 16, 0x00CF_9300_0000_FFFF).map_err(String::from)?;
    t.write_u64(GDT_ADDR + 24, 0x008F_8B00_0000_FFFF).map_err(String::from)?;
    t.write_u64(IDT_ADDR, 0).map_err(String::from)?;

    // Command line.
    let mut cl = cmdline.as_bytes().to_vec();
    cl.push(0);
    t.write(CMDLINE_ADDR, &cl).map_err(String::from)?;

    // Zero page / boot_params.
    let mut bp = alloc::vec![0u8; 4096];
    if let Some(hdr) = &plan.setup_header {
        bp[0x1F1..0x1F1 + hdr.len()].copy_from_slice(hdr);
    }
    bp[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
    bp[0x202..0x206].copy_from_slice(&0x5372_6448u32.to_le_bytes());
    if plan.setup_header.is_none() {
        bp[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
    }
    bp[0x210] = 0xFF; // type_of_loader: undefined loader
    bp[0x211] |= 0x01; // loadflags: LOADED_HIGH
    bp[0x218..0x21C].copy_from_slice(&(initrd_gpa as u32).to_le_bytes());
    bp[0x21C..0x220].copy_from_slice(&(initrd_size as u32).to_le_bytes());
    bp[0x228..0x22C].copy_from_slice(&(CMDLINE_ADDR as u32).to_le_bytes());
    bp[0x230..0x234].copy_from_slice(&0x0100_0000u32.to_le_bytes()); // kernel_alignment
    bp[0x238..0x23C].copy_from_slice(&((CMDLINE_MAX - 1) as u32).to_le_bytes());
    // e820 map.
    let mut e820: Vec<(u64, u64, u32)> = Vec::new();
    e820.push((0, 0x9FC00, E820_RAM));
    e820.push((0x9FC00, 0x100000 - 0x9FC00, E820_RESERVED));
    let low_end = mem_size.min(LOW_RAM_LIMIT);
    e820.push((0x100000, low_end - 0x100000, E820_RAM));
    if mem_size > LOW_RAM_LIMIT {
        e820.push((1 << 32, mem_size - LOW_RAM_LIMIT, E820_RAM));
    }
    bp[0x1E8] = e820.len() as u8;
    for (i, (addr, size, ty)) in e820.iter().enumerate() {
        let o = 0x2D0 + i * 20;
        bp[o..o + 8].copy_from_slice(&addr.to_le_bytes());
        bp[o + 8..o + 16].copy_from_slice(&size.to_le_bytes());
        bp[o + 16..o + 20].copy_from_slice(&ty.to_le_bytes());
    }
    t.write(ZERO_PAGE, &bp).map_err(String::from)?;

    // MP table: one processor, an ISA bus and the I/O APIC.  With it Linux
    // runs in symmetric I/O mode (fixed vectors, one EOI per interrupt, the
    // local APIC timer as tick source) instead of legacy PIC/PIT mode.
    write_mp_table(&mut t).map_err(String::from)?;

    log!(
        "linux: image '{}': {} entry {:#x}, kernel end {:#x}, initrd {:#x}+{} KiB, {} template pages for {} MiB",
        kernel.name,
        plan.format,
        plan.entry,
        plan.kernel_end,
        initrd_gpa,
        initrd_size >> 10,
        t.pages.len(),
        mem_size >> 20
    );
    Ok(t.finish())
}
