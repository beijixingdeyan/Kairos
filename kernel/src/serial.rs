//! 16550 UART serial console (COM1).
//!
//! The serial line is the kernel's most trustworthy output: it works even
//! before paging, before interrupts, and in panic paths. QEMU maps it to
//! stdio (`-serial mon:stdio`), which is also how our automated tests read
//! kernel output.

use spin::Mutex;
use uart_16550::SerialPort;
use x86_64::instructions::port::Port;

static SERIAL: Mutex<Option<SerialPort>> = Mutex::new(None);

/// Initialise COM1 at 38400 baud (any speed works for QEMU).
pub fn init() {
    // # Safety: writing to the standard 16550 I/O port range (0x3F8..).
    let mut port = unsafe { SerialPort::new(0x3F8) };
    port.init();
    *SERIAL.lock() = Some(port);
}

/// Write a raw byte; no-op until [`init`] ran.
pub fn put_byte(b: u8) {
    if let Some(port) = SERIAL.lock().as_mut() {
        port.send(b);
    }
}

/// Write a byte to the debug-exit "probe" port used by tests? Not needed.
pub fn _probe_byte(b: u8) {
    // exposed for completeness; unused today
    let mut p = Port::<u8>::new(0xe9);
    unsafe {
        p.write(b);
    }
}

/// Non-blocking read of one byte from the COM1 input queue (for the shell).
pub fn read_byte() -> Option<u8> {
    let mut port = SERIAL.lock();
    port.as_mut()?.try_receive().ok()
}

/// Block until a byte arrives (used by the shell when it must wait).
pub fn wait_byte() -> u8 {
    loop {
        if let Some(b) = read_byte() {
            return b;
        }
    }
}

/// Write a string (no newline added).
pub fn write_str(s: &str) {
    for &b in s.as_bytes() {
        put_byte(b);
    }
}

/// Write a line.
pub fn write_line(s: &str) {
    write_str(s);
    write_str("\r\n");
}

/// Underlying byte writer for `core::fmt`.
pub fn write_fmt(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    struct Sink;
    impl Write for Sink {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            write_str(s);
            Ok(())
        }
    }
    let _ = Sink.write_fmt(args);
}

#[cfg(test)]
pub fn test_echo() -> bool {
    let mut ok = true;
    let s = "serial:ok";
    write_line(s);
    // We cannot read back QEMU output from inside the guest; the assertion
    // happens in the host test harness which greps for the marker.
    ok &= s.len() > 0;
    ok
}