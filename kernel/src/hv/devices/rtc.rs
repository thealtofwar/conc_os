//! MC146818 CMOS real-time clock (ports 0x70/0x71).  The clock runs from a
//! fixed base date plus guest uptime; there is no host wall clock to consult.

#[derive(Clone)]
pub struct Rtc {
    index: u8,
    ram: [u8; 128],
    boot_tsc: u64,
    tsc_hz: u64,
}

/// 2026-09-04 12:00:00 UTC as seconds since the Unix epoch.
const BASE_EPOCH: u64 = 1_788_523_200;

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Rtc {
    pub fn new(tsc_hz: u64, boot_tsc: u64) -> Self {
        let mut ram = [0u8; 128];
        ram[0x0A] = 0x26; // 32 kHz, periodic rate 1 kHz, not updating
        ram[0x0B] = 0x02; // 24-hour, BCD (x86 Linux insists on BCD)
        ram[0x0D] = 0x80; // battery good
        ram[0x32] = 0x20; // century, BCD
        Rtc { index: 0, ram, boot_tsc, tsc_hz: tsc_hz.max(1) }
    }

    fn now_fields(&self, now: u64) -> [u8; 10] {
        let secs = BASE_EPOCH + (now.saturating_sub(self.boot_tsc)) / self.tsc_hz;
        let days = (secs / 86_400) as i64;
        let tod = secs % 86_400;
        let (y, m, d) = civil_from_days(days);
        let dow = ((days + 4) % 7) as u8; // 0 = Sunday
        let bcd = |v: u8| ((v / 10) << 4) | (v % 10);
        let mut f = [0u8; 10];
        f[0] = bcd((tod % 60) as u8);
        f[2] = bcd(((tod / 60) % 60) as u8);
        f[4] = bcd((tod / 3600) as u8);
        f[6] = bcd(dow + 1);
        f[7] = bcd(d as u8);
        f[8] = bcd(m as u8);
        f[9] = bcd((y % 100) as u8);
        f
    }

    pub fn io_read(&mut self, port: u16, now: u64) -> u8 {
        match port {
            0x70 => self.index,
            0x71 => {
                let idx = (self.index & 0x7F) as usize;
                match idx {
                    0 | 2 | 4 | 6 | 7 | 8 | 9 => self.now_fields(now)[idx],
                    0x0C => 0, // interrupt flags: none
                    _ => self.ram[idx],
                }
            }
            _ => 0xFF,
        }
    }

    pub fn io_write(&mut self, port: u16, v: u8) {
        match port {
            0x70 => self.index = v,
            0x71 => {
                let idx = (self.index & 0x7F) as usize;
                if idx != 0x0C && idx != 0x0D {
                    self.ram[idx] = v;
                }
            }
            _ => {}
        }
    }
}
