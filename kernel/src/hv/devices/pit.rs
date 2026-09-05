//! 8254 programmable interval timer.  Counts are derived from the host TSC
//! so they stay consistent with the guest-visible TSC that Linux compares
//! them against during calibration.

pub const PIT_HZ: u64 = 1_193_182;

#[derive(Clone, Copy, Default, Debug)]
struct Channel {
    /// Reload value; 0 means 65536.
    reload: u16,
    mode: u8,
    /// 1 = low byte, 2 = high byte, 3 = low then high.
    access: u8,
    bcd: bool,
    /// TSC at which counting started.
    start: u64,
    armed: bool,
    /// Pending low byte of a two-byte write.
    write_lo: Option<u8>,
    /// Next read returns the high byte.
    read_hi: bool,
    latched: Option<u16>,
    gate: bool,
    /// Number of whole periods already signalled (channel 0 IRQ).
    fired_periods: u64,
    fired_once: bool,
}

#[derive(Clone)]
pub struct Pit {
    ch: [Channel; 3],
    tsc_hz: u64,
    speaker: bool,
    refresh_toggle: bool,
    pub irq0_count: u64,
}

impl Pit {
    pub fn new(tsc_hz: u64) -> Self {
        let mut p = Pit {
            ch: [Channel::default(); 3],
            tsc_hz: tsc_hz.max(1),
            speaker: false,
            refresh_toggle: false,
            irq0_count: 0,
        };
        p.ch[0].gate = true;
        p.ch[1].gate = true;
        p.ch[2].gate = false;
        p
    }

    fn reload_of(c: &Channel) -> u64 {
        if c.reload == 0 {
            65536
        } else {
            c.reload as u64
        }
    }

    /// PIT ticks elapsed since the channel started counting.
    fn elapsed_ticks(&self, c: &Channel, now: u64) -> u64 {
        if !c.armed || now < c.start {
            return 0;
        }
        ((now - c.start) as u128 * PIT_HZ as u128 / self.tsc_hz as u128) as u64
    }

    /// Current count register value.
    fn count(&self, idx: usize, now: u64) -> u16 {
        let c = &self.ch[idx];
        if !c.armed {
            return c.reload;
        }
        let r = Self::reload_of(c);
        let e = self.elapsed_ticks(c, now);
        match c.mode {
            2 | 6 => (r - (e % r)) as u16,
            3 | 7 => {
                // Square wave: decrements by two, twice per period.
                let half = (r / 2).max(1);
                let phase = e % r;
                let in_half = phase % half;
                ((half - in_half) * 2) as u16
            }
            _ => {
                // Modes 0, 1, 4, 5: count down through zero and wrap.
                if e < r {
                    (r - e) as u16
                } else {
                    (0x1_0000 - ((e - r) % 0x1_0000)) as u16
                }
            }
        }
    }

    /// OUT pin level of a channel.
    fn output(&self, idx: usize, now: u64) -> bool {
        let c = &self.ch[idx];
        if !c.armed {
            return matches!(c.mode, 2 | 3 | 6 | 7) || c.mode == 0 && false;
        }
        let r = Self::reload_of(c);
        let e = self.elapsed_ticks(c, now);
        match c.mode {
            0 => e >= r,
            1 => e >= r,
            2 | 6 => (e % r) != r - 1,
            3 | 7 => (e % r) < (r + 1) / 2,
            _ => e < r, // strobe modes: low pulse at terminal count
        }
    }

    pub fn io_read(&mut self, port: u16, now: u64) -> u8 {
        match port {
            0x40..=0x42 => {
                let idx = (port - 0x40) as usize;
                let value = match self.ch[idx].latched {
                    Some(v) => v,
                    None => self.count(idx, now),
                };
                let c = &mut self.ch[idx];
                let byte = match c.access {
                    1 => {
                        c.latched = None;
                        (value & 0xFF) as u8
                    }
                    2 => {
                        c.latched = None;
                        (value >> 8) as u8
                    }
                    _ => {
                        if c.read_hi {
                            c.read_hi = false;
                            c.latched = None;
                            (value >> 8) as u8
                        } else {
                            c.read_hi = true;
                            (value & 0xFF) as u8
                        }
                    }
                };
                byte
            }
            0x61 => {
                self.refresh_toggle = !self.refresh_toggle;
                let mut v = 0u8;
                if self.ch[2].gate {
                    v |= 1;
                }
                if self.speaker {
                    v |= 2;
                }
                if self.refresh_toggle {
                    v |= 0x10;
                }
                if self.output(2, now) {
                    v |= 0x20;
                }
                v
            }
            _ => 0xFF,
        }
    }

    pub fn io_write(&mut self, port: u16, v: u8, now: u64) {
        match port {
            0x43 => {
                let sc = (v >> 6) as usize;
                if sc == 3 {
                    // Read-back command: latch counts for the selected channels.
                    for i in 0..3 {
                        if v & (2 << i) != 0 && v & 0x20 == 0 {
                            let cnt = self.count(i, now);
                            self.ch[i].latched = Some(cnt);
                        }
                    }
                    return;
                }
                let access = (v >> 4) & 3;
                if access == 0 {
                    let cnt = self.count(sc, now);
                    let c = &mut self.ch[sc];
                    if c.latched.is_none() {
                        c.latched = Some(cnt);
                    }
                    return;
                }
                let c = &mut self.ch[sc];
                c.access = access;
                c.mode = (v >> 1) & 7;
                c.bcd = v & 1 != 0;
                c.write_lo = None;
                c.read_hi = false;
                c.latched = None;
                c.armed = false;
                c.fired_periods = 0;
                c.fired_once = false;
            }
            0x40..=0x42 => {
                let idx = (port - 0x40) as usize;
                let gate = self.ch[idx].gate;
                let c = &mut self.ch[idx];
                let new_reload = match c.access {
                    1 => Some(v as u16),
                    2 => Some((v as u16) << 8),
                    _ => match c.write_lo.take() {
                        None => {
                            c.write_lo = Some(v);
                            None
                        }
                        Some(lo) => Some(lo as u16 | ((v as u16) << 8)),
                    },
                };
                if let Some(r) = new_reload {
                    c.reload = r;
                    c.fired_periods = 0;
                    c.fired_once = false;
                    if gate {
                        c.start = now;
                        c.armed = true;
                    } else {
                        c.armed = false;
                    }
                }
            }
            0x61 => {
                let was = self.ch[2].gate;
                self.ch[2].gate = v & 1 != 0;
                self.speaker = v & 2 != 0;
                if !was && self.ch[2].gate {
                    // Rising gate restarts the count.
                    let c = &mut self.ch[2];
                    c.start = now;
                    c.armed = true;
                    c.fired_once = false;
                }
            }
            _ => {}
        }
    }

    /// Advance channel 0; returns true when IRQ 0 should be pulsed.
    pub fn poll(&mut self, now: u64) -> bool {
        let c = self.ch[0];
        if !c.armed {
            return false;
        }
        let r = Self::reload_of(&c);
        let e = self.elapsed_ticks(&c, now);
        let fire = match c.mode {
            2 | 3 | 6 | 7 => {
                let periods = e / r;
                if periods > c.fired_periods {
                    self.ch[0].fired_periods = periods;
                    true
                } else {
                    false
                }
            }
            _ => {
                if e >= r && !c.fired_once {
                    self.ch[0].fired_once = true;
                    true
                } else {
                    false
                }
            }
        };
        if fire {
            self.irq0_count += 1;
        }
        fire
    }

    /// TSC at which channel 0 next raises IRQ 0.
    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        let c = &self.ch[0];
        if !c.armed {
            return None;
        }
        let r = Self::reload_of(c);
        let e = self.elapsed_ticks(c, now);
        let ticks_until = match c.mode {
            2 | 3 | 6 | 7 => r - (e % r),
            _ => {
                if c.fired_once {
                    return None;
                }
                r.saturating_sub(e).max(1)
            }
        };
        Some(now + (ticks_until as u128 * self.tsc_hz as u128 / PIT_HZ as u128) as u64 + 1)
    }

    pub fn debug_summary(&self, now: u64) -> alloc::string::String {
        alloc::format!(
            "ch0 mode={} reload={} armed={} count={} irq0={} | ch2 mode={} reload={} gate={} out={} count={}",
            self.ch[0].mode,
            self.ch[0].reload,
            self.ch[0].armed,
            self.count(0, now),
            self.irq0_count,
            self.ch[2].mode,
            self.ch[2].reload,
            self.ch[2].gate,
            self.output(2, now),
            self.count(2, now)
        )
    }
}
