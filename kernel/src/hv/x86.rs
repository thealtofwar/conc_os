//! Guest-side x86 helpers: virtual-address translation through the guest's
//! own page tables, instruction fetch, and a decoder for the MOV forms that
//! touch memory-mapped device registers.
//!
//! Only 64-bit mode is supported.  The decoder handles the encodings
//! compilers emit for `readl`/`writel` style accesses: MOV r/m,r and MOV r,r/m
//! (8/16/32/64-bit), MOV r/m,imm, and MOVZX loads, with any ModRM/SIB
//! addressing form including RIP-relative.

#![allow(dead_code)]

use super::memory::GuestMemory;
use super::svm::GuestRegs;

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Translate a guest virtual address via the guest's 4-level page tables.
pub async fn gva_to_gpa(mem: &mut GuestMemory, cr3: u64, va: u64) -> Result<u64, &'static str> {
    let mut table = cr3 & ADDR_MASK;
    let mut level = 3i32;
    while level >= 0 {
        let idx = (va >> (12 + 9 * level as u64)) & 0x1FF;
        let mut e = [0u8; 8];
        mem.read(table + idx * 8, &mut e).await?;
        let entry = u64::from_le_bytes(e);
        if entry & 1 == 0 {
            return Err("guest page not present");
        }
        if level > 0 && level < 3 && entry & 0x80 != 0 {
            let shift = 12 + 9 * level as u64;
            let mask = (1u64 << shift) - 1;
            return Ok(((entry & ADDR_MASK) & !mask) | (va & mask));
        }
        table = entry & ADDR_MASK;
        level -= 1;
    }
    Ok(table | (va & 0xFFF))
}

/// Fetch up to 15 instruction bytes at guest virtual `rip`.  Returns the
/// bytes and how many are valid (fewer if the next page is unmapped).
pub async fn fetch(mem: &mut GuestMemory, cr3: u64, rip: u64) -> Result<([u8; 15], usize), &'static str> {
    let mut buf = [0u8; 15];
    let mut got = 0usize;
    while got < 15 {
        let va = rip + got as u64;
        let gpa = match gva_to_gpa(mem, cr3, va).await {
            Ok(g) => g,
            Err(e) if got > 0 => {
                let _ = e;
                break;
            }
            Err(e) => return Err(e),
        };
        let n = ((4096 - (gpa & 0xFFF)) as usize).min(15 - got);
        mem.read(gpa, &mut buf[got..got + n]).await?;
        got += n;
    }
    Ok((buf, got))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Load from memory into GPR `reg`.
    Load { reg: u8 },
    /// Load with zero extension from `from_size` bytes into GPR `reg`.
    LoadZx { reg: u8, from_size: u8 },
    /// Store GPR `reg` to memory.
    Store { reg: u8 },
    /// Store an immediate.
    StoreImm { imm: u64 },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MemOperand {
    pub base: Option<u8>,
    pub index: Option<u8>,
    pub scale: u8,
    pub disp: i64,
    pub rip_relative: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MovInsn {
    pub len: u8,
    /// Operand size in bytes (1, 2, 4, 8).
    pub size: u8,
    pub access: Access,
    pub mem: MemOperand,
}

fn imm_sx(b: &[u8], i: usize, n: usize) -> Result<i64, &'static str> {
    if i + n > b.len() {
        return Err("truncated immediate");
    }
    Ok(match n {
        1 => b[i] as i8 as i64,
        2 => i16::from_le_bytes([b[i], b[i + 1]]) as i64,
        4 => i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as i64,
        _ => return Err("bad immediate size"),
    })
}

/// Decode a memory-accessing MOV at the start of `b`.
pub fn decode_mov(b: &[u8]) -> Result<MovInsn, &'static str> {
    let mut i = 0usize;
    let mut opsize16 = false;
    // Legacy prefixes.
    loop {
        match b.get(i) {
            Some(0x66) => {
                opsize16 = true;
                i += 1;
            }
            Some(0x67) => return Err("address-size override not supported"),
            Some(0xF0) | Some(0xF2) | Some(0xF3) | Some(0x26) | Some(0x2E) | Some(0x36) | Some(0x3E) | Some(0x64) | Some(0x65) => {
                i += 1;
            }
            _ => break,
        }
        if i > 4 {
            return Err("too many prefixes");
        }
    }
    let mut rex = 0u8;
    if let Some(&p) = b.get(i) {
        if p & 0xF0 == 0x40 {
            rex = p;
            i += 1;
        }
    }
    let rex_w = rex & 8 != 0;
    let rex_r = ((rex & 4) >> 2) << 3;
    let rex_x = ((rex & 2) >> 1) << 3;
    let rex_b = (rex & 1) << 3;
    let osz: u8 = if rex_w {
        8
    } else if opsize16 {
        2
    } else {
        4
    };

    let op = *b.get(i).ok_or("truncated opcode")?;
    i += 1;
    let mut op2 = 0u8;
    if op == 0x0F {
        op2 = *b.get(i).ok_or("truncated opcode")?;
        i += 1;
    }

    #[derive(PartialEq)]
    enum Kind {
        Load,
        LoadZx(u8),
        Store,
        StoreImm,
    }
    let (size, kind) = match (op, op2) {
        (0x88, _) => (1, Kind::Store),
        (0x89, _) => (osz, Kind::Store),
        (0x8A, _) => (1, Kind::Load),
        (0x8B, _) => (osz, Kind::Load),
        (0xC6, _) => (1, Kind::StoreImm),
        (0xC7, _) => (osz, Kind::StoreImm),
        (0x0F, 0xB6) => (osz, Kind::LoadZx(1)),
        (0x0F, 0xB7) => (osz, Kind::LoadZx(2)),
        _ => return Err("unsupported opcode for MMIO emulation"),
    };

    let modrm = *b.get(i).ok_or("truncated modrm")?;
    i += 1;
    let md = modrm >> 6;
    let reg = ((modrm >> 3) & 7) | rex_r;
    let rm = modrm & 7;
    if md == 3 {
        return Err("register-only operand");
    }
    if kind == Kind::StoreImm && (modrm >> 3) & 7 != 0 {
        return Err("unsupported group opcode");
    }
    if size == 1 && rex == 0 && matches!(kind, Kind::Load | Kind::Store) && reg >= 4 {
        return Err("high byte register not supported");
    }

    let mut mem = MemOperand { base: None, index: None, scale: 1, disp: 0, rip_relative: false };
    let mut disp_size = match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    if rm == 4 {
        let sib = *b.get(i).ok_or("truncated sib")?;
        i += 1;
        let scale = 1u8 << (sib >> 6);
        let index = ((sib >> 3) & 7) | rex_x;
        let base = (sib & 7) | rex_b;
        if index != 4 {
            mem.index = Some(index);
            mem.scale = scale;
        }
        if (sib & 7) == 5 && md == 0 {
            disp_size = 4;
        } else {
            mem.base = Some(base);
        }
    } else if rm == 5 && md == 0 {
        mem.rip_relative = true;
        disp_size = 4;
    } else {
        mem.base = Some(rm | rex_b);
    }
    if disp_size > 0 {
        mem.disp = imm_sx(b, i, disp_size)?;
        i += disp_size;
    }

    let access = match kind {
        Kind::Load => Access::Load { reg },
        Kind::LoadZx(from) => Access::LoadZx { reg, from_size: from },
        Kind::Store => Access::Store { reg },
        Kind::StoreImm => {
            let n = (size as usize).min(4);
            let imm = imm_sx(b, i, n)?;
            i += n;
            let mask = if size == 8 { u64::MAX } else { (1u64 << (size as u32 * 8)) - 1 };
            Access::StoreImm { imm: (imm as u64) & mask }
        }
    };
    Ok(MovInsn { len: i as u8, size, access, mem })
}

/// Read general purpose register `idx` (0 = rax ... 15 = r15).
pub fn gpr(idx: u8, regs: &GuestRegs, rax: u64, rsp: u64) -> u64 {
    match idx {
        0 => rax,
        1 => regs.rcx,
        2 => regs.rdx,
        3 => regs.rbx,
        4 => rsp,
        5 => regs.rbp,
        6 => regs.rsi,
        7 => regs.rdi,
        8 => regs.r8,
        9 => regs.r9,
        10 => regs.r10,
        11 => regs.r11,
        12 => regs.r12,
        13 => regs.r13,
        14 => regs.r14,
        _ => regs.r15,
    }
}

/// Write `v` (of `size` bytes) into register `idx` with x86-64 rules:
/// 32-bit writes zero the upper half, 8/16-bit writes merge.
pub fn set_gpr(idx: u8, v: u64, size: u8, regs: &mut GuestRegs, rax: &mut u64, rsp: &mut u64) {
    let cur = gpr(idx, regs, *rax, *rsp);
    let nv = match size {
        1 => (cur & !0xFF) | (v & 0xFF),
        2 => (cur & !0xFFFF) | (v & 0xFFFF),
        4 => v & 0xFFFF_FFFF,
        _ => v,
    };
    let slot: &mut u64 = match idx {
        0 => rax,
        1 => &mut regs.rcx,
        2 => &mut regs.rdx,
        3 => &mut regs.rbx,
        4 => rsp,
        5 => &mut regs.rbp,
        6 => &mut regs.rsi,
        7 => &mut regs.rdi,
        8 => &mut regs.r8,
        9 => &mut regs.r9,
        10 => &mut regs.r10,
        11 => &mut regs.r11,
        12 => &mut regs.r12,
        13 => &mut regs.r13,
        14 => &mut regs.r14,
        _ => &mut regs.r15,
    };
    *slot = nv;
}

/// Effective address of a decoded memory operand.
pub fn effective_address(insn: &MovInsn, regs: &GuestRegs, rax: u64, rsp: u64, rip: u64) -> u64 {
    let m = &insn.mem;
    let mut ea = m.disp as u64;
    if let Some(b) = m.base {
        ea = ea.wrapping_add(gpr(b, regs, rax, rsp));
    }
    if let Some(ix) = m.index {
        ea = ea.wrapping_add(gpr(ix, regs, rax, rsp).wrapping_mul(m.scale as u64));
    }
    if m.rip_relative {
        ea = ea.wrapping_add(rip + insn.len as u64);
    }
    ea
}

/// Value to be stored for a Store/StoreImm access, masked to the size.
pub fn store_value(insn: &MovInsn, regs: &GuestRegs, rax: u64, rsp: u64) -> u64 {
    let v = match insn.access {
        Access::Store { reg } => gpr(reg, regs, rax, rsp),
        Access::StoreImm { imm } => imm,
        _ => 0,
    };
    if insn.size == 8 {
        v
    } else {
        v & ((1u64 << (insn.size as u32 * 8)) - 1)
    }
}
