use core::fmt::{self, Write};

use spin::Mutex;
use volatile::Volatile;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ColorCode(u8);

impl ColorCode {
    pub fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

struct VGABuffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

struct ScreenPos {
    row: usize,
    col: usize,
}

impl ScreenPos {
    pub fn new(row: usize, col: usize) -> ScreenPos {
        ScreenPos { row, col }
    }

    pub fn incr_col(&mut self) {
        self.col += 1;
        if self.col == BUFFER_WIDTH {
            self.incr_row();
        }
    }

    pub fn incr_row(&mut self) {
        self.row += 1;
        if self.col == BUFFER_HEIGHT {
            self.row = 0;
        }
        self.col = 0;
    }
}

pub struct Writer {
    position: ScreenPos,
    color_code: ColorCode,
    buffer: &'static mut VGABuffer,
}

impl Writer {
    pub fn new(color_code: ColorCode) -> Writer {
        Writer {
            position: ScreenPos::new(0, 0),
            color_code,
            buffer: unsafe { &mut *(0xb8000 as *mut VGABuffer) },
        }
    }
}

impl Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.as_bytes() {
            if *byte == b'\n' {
                self.position.incr_row();
                continue;
            }
            self.buffer.chars[self.position.row][self.position.col].write(ScreenChar {
                ascii_character: *byte,
                color_code: self.color_code,
            });
            self.position.incr_col();
        }
        Ok(())
    }
}

use lazy_static::lazy_static;

lazy_static! {
    pub static ref WRITER: Mutex<Writer> =
        Mutex::new(Writer::new(ColorCode::new(Color::LightBlue, Color::Black)));
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::vga::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    WRITER.lock().write_fmt(args).unwrap();
}
