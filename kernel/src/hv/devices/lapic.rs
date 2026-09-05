//! Local APIC model (xAPIC register file; also reachable through the x2APIC
//! MSR window).  Single CPU: IPIs to self are the only IPIs that exist.
//!
//! The timer counts in a virtual bus clock of `APIC_HZ` derived from the
//! host TSC; Linux calibrates against it, so the exact frequency is not
//! important but the countdown must be consistent with the guest-visible TSC.

use crate::time;

pub const APIC_BASE: u64 = 0xFEE0_0000;
pub const APIC_HZ: u64 = 1_000_000_000;

pub const REG_ID: u32 = 0x20;
pub const REG_VERSION: u32 = 0x30;
pub const REG_TPR: u32 = 0x80;
pub const REG_APR: u32 = 0x90;
pub const REG_PPR: u32 = 0xA0;
pub const REG_EOI: u32 = 0xB0;
pub const REG_RRD: u32 = 0xC0;
pub const REG_LDR: u32 = 0xD0;
pub const REG_DFR: u32 = 0xE0;
pub const REG_SVR: u32 = 0xF0;
pub const REG_ISR: u32 = 0x100;
pub const REG_TMR: u32 = 0x180;
pub const REG_IRR: u32 = 0x200;
pub const REG_ESR: u32 = 0x280;
pub const REG_LVT_CMCI: u32 = 0x2F0;
pub const REG_ICR_LO: u32 = 0x300;
pub const REG_ICR_HI: u32 = 0x310;
pub const REG_LVT_TIMER: u32 = 0x320;
pub const REG_LVT_THERMAL: u32 = 0x330;
pub const REG_LVT_PERF: u32 = 0x340;
pub const REG_LVT_LINT0: u32 = 0x350;
pub const REG_LVT_LINT1: u32 = 0x360;
pub const REG_LVT_ERROR: u32 = 0x370;
pub const REG_TIMER_ICR: u32 = 0x380;
pub const REG_TIMER_CCR: u32 = 0x390;
pub const REG_TIMER_DCR: u32 = 0x3E0;
pub const REG_SELF_IPI: u32 = 0x3F0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_DM_MASK: u32 = 0x700;
const LVT_DM_EXTINT: u32 = 0x700;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimerMode {
    OneShot,
    Periodic,
    TscDeadline,
}

#[derive(Clone)]
pub struct Lapic {
    pub id: u32,
    tpr: u32,
    ldr: u32,
    dfr: u32,
    svr: u32,
    esr: u32,
    icr_hi: u32,
    icr_lo: u32,
    lvt: [u32; 7], // cmci, timer, thermal, perf, lint0, lint1, error
    isr: [u32; 8],
    irr: [u32; 8],
    tmr: [u32; 8],
    // Timer.
    timer_icr: u32,
    timer_dcr: u32,
    timer_start: u64,
    timer_period_tsc: u64,
    timer_next_fire: u64,
    timer_armed: bool,
    tsc_deadline: u64,
    /// ExtINT request from the 8259 (level).
    extint_pending: bool,
    pub tsc_hz: u64,
    pub timer_fired: u64,
    pub x2apic: bool,
    /// Vector of the last EOI'd level-triggered interrupt, for the I/O APIC.
    pub last_level_eoi: Option<u8>,
}

impl Lapic {
    pub fn new(tsc_hz: u64) -> Self {
        Lapic {
            id: 0,
            tpr: 0,
            ldr: 0,
            dfr: 0xFFFF_FFFF,
            svr: 0xFF,
            esr: 0,
            icr_hi: 0,
            icr_lo: 0,
            lvt: [LVT_MASKED; 7],
            isr: [0; 8],
            irr: [0; 8],
            tmr: [0; 8],
            timer_icr: 0,
            timer_dcr: 0,
            timer_start: 0,
            timer_period_tsc: 0,
            timer_next_fire: 0,
            timer_armed: false,
            tsc_deadline: 0,
            extint_pending: false,
            tsc_hz,
            timer_fired: 0,
            x2apic: false,
            last_level_eoi: None,
        }
    }

    pub fn software_enabled(&self) -> bool {
        self.svr & 0x100 != 0
    }

    fn divider(&self) -> u64 {
        let d = (self.timer_dcr & 3) | ((self.timer_dcr >> 1) & 4);
        match d {
            0 => 2,
            1 => 4,
            2 => 8,
            3 => 16,
            4 => 32,
            5 => 64,
            6 => 128,
            _ => 1,
        }
    }

    fn timer_mode(&self) -> TimerMode {
        match (self.lvt[1] >> 17) & 3 {
            1 => TimerMode::Periodic,
            2 => TimerMode::TscDeadline,
            _ => TimerMode::OneShot,
        }
    }

    /// TSC ticks per APIC timer tick at the current divider.
    fn tsc_per_tick(&self) -> u64 {
        (self.tsc_hz * self.divider()).max(1) / APIC_HZ.max(1)
    }

    fn arm_timer(&mut self, now: u64) {
        if self.timer_icr == 0 || self.timer_mode() == TimerMode::TscDeadline {
            self.timer_armed = false;
            return;
        }
        self.timer_start = now;
        self.timer_period_tsc = (self.timer_icr as u64 * self.tsc_per_tick().max(1)).max(1);
        self.timer_next_fire = now + self.timer_period_tsc;
        self.timer_armed = true;
    }

    /// Current count register value.
    fn current_count(&self, now: u64) -> u32 {
        if !self.timer_armed || self.timer_icr == 0 {
            return 0;
        }
        let elapsed_ticks = (now.saturating_sub(self.timer_start)) / self.tsc_per_tick().max(1);
        match self.timer_mode() {
            TimerMode::Periodic => {
                let icr = self.timer_icr as u64;
                (icr - (elapsed_ticks % icr)) as u32
            }
            _ => (self.timer_icr as u64).saturating_sub(elapsed_ticks) as u32,
        }
    }

    fn set_irr(&mut self, vector: u8) {
        if vector < 16 {
            return; // illegal vector
        }
        self.irr[(vector >> 5) as usize] |= 1 << (vector & 31);
    }

    fn highest(bits: &[u32; 8]) -> Option<u8> {
        for i in (0..8).rev() {
            if bits[i] != 0 {
                return Some((i as u8) * 32 + (31 - bits[i].leading_zeros() as u8));
            }
        }
        None
    }

    fn ppr(&self) -> u32 {
        let isrv = Self::highest(&self.isr).map(|v| v as u32 & 0xF0).unwrap_or(0);
        if (self.tpr & 0xF0) >= isrv {
            self.tpr
        } else {
            isrv
        }
    }

    /// Advance timers; queue interrupts that became due.
    pub fn poll(&mut self, now: u64) {
        if self.timer_armed && now >= self.timer_next_fire {
            match self.timer_mode() {
                TimerMode::Periodic => {
                    // Coalesce missed periods instead of bursting.
                    self.timer_next_fire = now + self.timer_period_tsc;
                }
                _ => self.timer_armed = false,
            }
            self.fire_lvt(1);
        }
        if self.tsc_deadline != 0 && now >= self.tsc_deadline {
            self.tsc_deadline = 0;
            self.fire_lvt(1);
        }
    }

    fn fire_lvt(&mut self, idx: usize) {
        let lvt = self.lvt[idx];
        if lvt & LVT_MASKED != 0 || !self.software_enabled() {
            return;
        }
        self.set_irr(lvt as u8);
        if idx == 1 {
            self.timer_fired += 1;
        }
    }

    /// Next TSC at which the timer needs attention.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut d: Option<u64> = None;
        if self.timer_armed {
            d = Some(self.timer_next_fire);
        }
        if self.tsc_deadline != 0 {
            d = Some(d.map_or(self.tsc_deadline, |x| x.min(self.tsc_deadline)));
        }
        d
    }

    /// Level of the ExtINT input (LINT0 wired to the 8259 INTR output).
    pub fn set_extint(&mut self, asserted: bool) {
        self.extint_pending = asserted;
    }

    /// Does the guest's APIC configuration route the 8259 output through?
    /// True in virtual-wire mode (LVT0 = ExtINT) or while the APIC is
    /// software-disabled (legacy PIC mode).
    pub fn extint_deliverable(&self) -> bool {
        if !self.extint_pending {
            return false;
        }
        if !self.software_enabled() {
            return true;
        }
        let lint0 = self.lvt[4];
        lint0 & LVT_MASKED == 0 && lint0 & LVT_DM_MASK == LVT_DM_EXTINT
    }

    /// Highest-priority fixed interrupt that may be delivered now.
    pub fn pending_fixed(&self) -> Option<u8> {
        let v = Self::highest(&self.irr)?;
        if (v as u32 & 0xF0) > (self.ppr() & 0xF0) {
            Some(v)
        } else {
            None
        }
    }

    /// Deliver a fixed interrupt: IRR -> ISR.
    pub fn ack_fixed(&mut self, vector: u8) {
        let (i, b) = ((vector >> 5) as usize, 1u32 << (vector & 31));
        self.irr[i] &= !b;
        self.isr[i] |= b;
    }

    fn eoi(&mut self) {
        if let Some(v) = Self::highest(&self.isr) {
            let (i, b) = ((v >> 5) as usize, 1u32 << (v & 31));
            self.isr[i] &= !b;
            if self.tmr[i] & b != 0 {
                // Level-triggered: the I/O APIC wants to know.
                self.tmr[i] &= !b;
                self.last_level_eoi = Some(v);
            }
        }
    }

    /// Fixed-delivery interrupt from a device or self-IPI.
    pub fn inject_vector(&mut self, vector: u8) {
        let (i, b) = ((vector >> 5) as usize, 1u32 << (vector & 31));
        self.tmr[i] &= !b;
        self.set_irr(vector);
    }

    /// Level-triggered fixed interrupt (sets the trigger-mode bit so the
    /// EOI is reported back to the I/O APIC).
    pub fn inject_vector_level(&mut self, vector: u8) {
        let (i, b) = ((vector >> 5) as usize, 1u32 << (vector & 31));
        self.tmr[i] |= b;
        self.set_irr(vector);
    }

    /// Vector programmed in the LVT timer entry.
    pub fn timer_vector(&self) -> u8 {
        (self.lvt[1] & 0xFF) as u8
    }

    pub fn read(&self, reg: u32, now: u64) -> u32 {
        match reg {
            REG_ID => {
                if self.x2apic {
                    self.id
                } else {
                    self.id << 24
                }
            }
            // Version 0x14, 6 LVT entries (max index 5), EOI-broadcast suppression not supported.
            REG_VERSION => 0x0005_0014,
            REG_TPR => self.tpr,
            REG_APR => 0,
            REG_PPR => self.ppr(),
            REG_RRD => 0,
            REG_LDR => self.ldr,
            REG_DFR => self.dfr,
            REG_SVR => self.svr,
            0x100..=0x170 if reg & 0xF == 0 => self.isr[((reg - REG_ISR) >> 4) as usize],
            0x180..=0x1F0 if reg & 0xF == 0 => self.tmr[((reg - REG_TMR) >> 4) as usize],
            0x200..=0x270 if reg & 0xF == 0 => self.irr[((reg - REG_IRR) >> 4) as usize],
            REG_ESR => self.esr,
            REG_LVT_CMCI => self.lvt[0],
            REG_ICR_LO => self.icr_lo & !(1 << 12), // never busy
            REG_ICR_HI => self.icr_hi,
            REG_LVT_TIMER => self.lvt[1],
            REG_LVT_THERMAL => self.lvt[2],
            REG_LVT_PERF => self.lvt[3],
            REG_LVT_LINT0 => self.lvt[4],
            REG_LVT_LINT1 => self.lvt[5],
            REG_LVT_ERROR => self.lvt[6],
            REG_TIMER_ICR => self.timer_icr,
            REG_TIMER_CCR => self.current_count(now),
            REG_TIMER_DCR => self.timer_dcr,
            _ => 0,
        }
    }

    pub fn write(&mut self, reg: u32, v: u32, now: u64) {
        match reg {
            REG_ID => self.id = if self.x2apic { v } else { v >> 24 },
            REG_TPR => self.tpr = v & 0xFF,
            REG_EOI => self.eoi(),
            REG_LDR => self.ldr = v,
            REG_DFR => self.dfr = v | 0x0FFF_FFFF,
            REG_SVR => {
                let was = self.software_enabled();
                self.svr = v & 0x3FF;
                if was && !self.software_enabled() {
                    // Disabling masks all LVTs, as the architecture requires.
                    for l in self.lvt.iter_mut() {
                        *l |= LVT_MASKED;
                    }
                }
            }
            REG_ESR => self.esr = 0,
            REG_LVT_CMCI => self.lvt[0] = v,
            REG_ICR_LO => {
                self.icr_lo = v & !(1 << 12);
                self.send_ipi(v);
            }
            REG_ICR_HI => self.icr_hi = v,
            REG_LVT_TIMER => {
                let old_mode = self.timer_mode();
                self.lvt[1] = v;
                if self.timer_mode() != old_mode {
                    // Switching mode disarms the current countdown.
                    self.timer_armed = false;
                    if self.timer_mode() != TimerMode::TscDeadline {
                        self.tsc_deadline = 0;
                    }
                }
            }
            REG_LVT_THERMAL => self.lvt[2] = v,
            REG_LVT_PERF => self.lvt[3] = v,
            REG_LVT_LINT0 => self.lvt[4] = v,
            REG_LVT_LINT1 => self.lvt[5] = v,
            REG_LVT_ERROR => self.lvt[6] = v,
            REG_TIMER_ICR => {
                self.timer_icr = v;
                self.arm_timer(now);
            }
            REG_TIMER_DCR => {
                self.timer_dcr = v & 0xB;
                if self.timer_armed {
                    // Re-arm with the remaining count at the new rate.
                    let remaining = self.current_count(now);
                    self.timer_icr = if self.timer_mode() == TimerMode::Periodic { self.timer_icr } else { remaining };
                    self.arm_timer(now);
                }
            }
            REG_SELF_IPI => self.set_irr(v as u8),
            _ => {}
        }
    }

    fn send_ipi(&mut self, icr: u32) {
        let vector = icr as u8;
        let delivery = (icr >> 8) & 7;
        let shorthand = (icr >> 18) & 3;
        let dest = self.icr_hi >> 24;
        // Single CPU: anything aimed at us (or "self"/"all") is a self-IPI.
        let to_us = shorthand == 1 || shorthand == 2 || (shorthand == 0 && (dest == self.id || dest == 0xFF));
        if !to_us {
            return;
        }
        match delivery {
            0 | 1 => self.set_irr(vector), // fixed / lowest priority
            _ => {}                        // SMI, NMI, INIT, SIPI: ignored on a single CPU
        }
    }

    /// IA32_TSC_DEADLINE MSR.
    pub fn set_tsc_deadline(&mut self, v: u64) {
        if self.timer_mode() == TimerMode::TscDeadline {
            self.tsc_deadline = v;
        }
    }

    pub fn tsc_deadline(&self) -> u64 {
        self.tsc_deadline
    }

    pub fn debug_summary(&self, now: u64) -> alloc::string::String {
        alloc::format!(
            "svr={:#x} tpr={:#x} lvtt={:#x} icr={} ccr={} dcr={:#x} lint0={:#x} irr={:?} isr={:?} fired={}",
            self.svr,
            self.tpr,
            self.lvt[1],
            self.timer_icr,
            self.current_count(now),
            self.timer_dcr,
            self.lvt[4],
            Self::highest(&self.irr),
            Self::highest(&self.isr),
            self.timer_fired
        )
    }
}

#[allow(dead_code)]
pub fn now() -> u64 {
    time::now()
}
