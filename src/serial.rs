use core::fmt::{self, Write};
use lazy_static::lazy_static; // Or spin::Lazy
use spin::Mutex;
use uart_16550::{Config, Uart16550Tty, backend::PioBackend};

lazy_static! {
    /// Global interface for the kernel's serial terminal
    pub static ref SERIAL_TTY: Mutex<Uart16550Tty<PioBackend>> = {
        // Replace with new_mmio if targeting an MMIO architecture
        let tty = unsafe {
            Uart16550Tty::new_port(0x3F8, Config::default())
                .expect("Failed to init serial TTY")
        };
        Mutex::new(tty)
    };
}

pub fn print(args: fmt::Arguments) {
    use core::fmt::Write;
    // Lock the global TTY instance and write to it
    SERIAL_TTY.lock().write_fmt(args).unwrap();
}

pub enum TTYErr {
    BufferTooSmall,
    SerialErr,
}

impl From<core::fmt::Error> for TTYErr {
    fn from(_value: core::fmt::Error) -> Self {
        Self::SerialErr
    }
}

pub fn readline(buffer: &mut [u8]) -> Result<usize, TTYErr> {
    let mut count = 0;

    loop {
        // 1. Poll for an incoming byte
        let byte = match SERIAL_TTY.lock().inner_mut().try_receive_byte() {
            Ok(b) => b,
            Err(_) => continue, // Keep polling if nothing is ready
        };

        match byte {
            // Handle Enter key (Line Feed or Carriage Return depending on host terminal)
            b'\n' | b'\r' => {
                // Print a newline back to the user so their cursor advances
                SERIAL_TTY.lock().write_char('\n')?;
                break;
            }

            // Handle Backspace (ASCII 8 or ASCII 127/Delete)
            8 | 127
                if count > 0 => {
                    count -= 1;
                    // Erase character from terminal: Move left, print space, move left again
                    print(format_args!("{}{}{}", 8 as char, ' ', 8 as char));
                }

            // Handle standard printable ASCII characters
            32..=126 => {
                // Ensure we don't overflow the provided buffer
                if count < buffer.len() {
                    buffer[count] = byte;
                    count += 1;

                    // Echo the character back to the screen
                    print(format_args!("{}", byte as char));
                } else {
                    return Err(TTYErr::BufferTooSmall);
                }
            }

            // Ignore control characters (like arrow keys, tabs, etc.) for now
            _ => {}
        }
    }

    Ok(count)
}
