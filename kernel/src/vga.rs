//! VGA text-mode screen (80×25).
//!
//! The bootloader leaves the display in text mode for BIOS boots; we map the
//! physical buffer at 0xB8000 (see `memory/paging.rs`) and expose a tiny
//! safe screen abstraction. The serial line remains the primary output; the
//! screen is mainly for humans watching the VM window.

use core::fmt;
use spin::Mutex;

const BUFFER_ADDRESS: usize = 0xB8000;
const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;
const CELLS: usize = BUFFER_WIDTH * BUFFER_HEIGHT;

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    const fn new(fg: Color, bg: Color) -> Self {
        Self((bg as u8) << 4 | (fg as u8))
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct ScreenChar {
    ascii: u8,
    color_code: ColorCode,
}

const BLANK: ScreenChar = ScreenChar {
    ascii: b' ',
    color_code: ColorCode::new(Color::LightGray, Color::Black),
};

static WRITER: Mutex<Option<Screen>> = Mutex::new(None);

pub struct Screen {
    column: usize,
    color: ColorCode,
    // Boot-time raw pointer into the VGA window (mapped by `memory::init`);
    // constructed lazily at runtime so the const evaluator never sees a
    // bare integer provenance issue for 0xB8000.
    buffer: *mut ScreenChar,
}

// # Safety: the WRITER mutex serialises every access (single CPU kernel);
// the raw buffer pointer is never leaked.
unsafe impl Send for Screen {}

impl Screen {
    fn new() -> Self {
        Self {
            column: 0,
            color: ColorCode::new(Color::LightGray, Color::Black),
            buffer: BUFFER_ADDRESS as *mut ScreenChar,
        }
    }

    /// Volatile write of one cell (prevents the compiler from eliding
    /// MMIO-style stores).
    fn write_cell(&mut self, idx: usize, c: ScreenChar) {
        // # Safety: idx < CELLS; the VGA window is mapped and exclusively
        // owned via the WRITER mutex.
        unsafe {
            core::ptr::write_volatile(self.buffer.add(idx), c);
        }
    }

    fn read_cell(&mut self, idx: usize) -> ScreenChar {
        // # Safety: idx < CELLS; see write_cell.
        unsafe { core::ptr::read_volatile(self.buffer.add(idx)) }
    }

    /// Must be called after `memory::init` mapped the VGA page.
    pub fn init() {
        let mut guard = WRITER.lock();
        if guard.is_none() {
            *guard = Some(Screen::new());
        }
        guard.as_mut().unwrap().clear();
    }

    fn clear(&mut self) {
        for idx in 0..CELLS {
            self.write_cell(idx, BLANK);
        }
        self.column = 0;
    }

    fn newline(&mut self) {
        self.column = 0;
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let c = self.read_cell(row * BUFFER_WIDTH + col);
                self.write_cell((row - 1) * BUFFER_WIDTH + col, c);
            }
        }
        for col in 0..BUFFER_WIDTH {
            self.write_cell((BUFFER_HEIGHT - 1) * BUFFER_WIDTH + col, BLANK);
        }
    }

    fn put_char(&mut self, c: char) {
        let ascii = if (c as u32) < 256 { c as u8 } else { b'?' };
        match c {
            '\n' => self.newline(),
            '\r' => self.column = 0,
            _ => {
                if self.column >= BUFFER_WIDTH {
                    self.newline();
                }
                self.write_cell(
                    (BUFFER_HEIGHT - 1) * BUFFER_WIDTH + self.column,
                    ScreenChar {
                        ascii,
                        color_code: self.color,
                    },
                );
                self.column += 1;
            }
        }
    }

    fn set_color(&mut self, fg: Color) {
        self.color = ColorCode::new(fg, Color::Black);
    }
}

impl fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.put_char(c);
        }
        Ok(())
    }
}

/// Put a formatted line on screen (also mirrored to serial by the logger).
pub fn print(args: fmt::Arguments<'_>) {
    use fmt::Write;
    if let Some(s) = WRITER.lock().as_mut() {
        let _ = s.write_fmt(args);
    }
}

/// Init once the VGA page is mapped.
pub fn init() {
    Screen::init();
}

/// Show a panic banner in red.
pub fn panic_message(info: &core::panic::PanicInfo) {
    use fmt::Write;
    let mut guard = WRITER.lock();
    if guard.is_none() {
        *guard = Some(Screen::new());
    }
    let s = guard.as_mut().unwrap();
    s.set_color(Color::Red);
    s.clear();
    let _ = write!(s, "PANIC: {info}");
    let _ = s.write_str("[kairos halted]");
}

pub fn test_echo() -> bool {
    use fmt::Write;
    let mut guard = WRITER.lock();
    if guard.is_none() {
        *guard = Some(Screen::new());
    }
    let _ = guard.as_mut().unwrap().write_str("vga:ok");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_heights_are_correct() {
        assert_eq!(BUFFER_WIDTH, 80);
        assert_eq!(BUFFER_HEIGHT, 25);
    }

    #[test]
    fn color_code_packs() {
        let c = ColorCode::new(Color::Red, Color::Blue);
        assert_eq!(c.0, (Color::Blue as u8) << 4 | Color::Red as u8);
    }
}