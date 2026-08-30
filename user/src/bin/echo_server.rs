//! IPC demo — echo server: receives messages and echoes them back.
#![no_std]
#![no_main]

use kairos;
use kairos::Message;

/// # Safety: raw entry point; the kernel passes the channel slot in `rdi`
/// (slot = arg, 1-based).
#[unsafe(no_mangle)]
pub extern "C" fn _start(arg: usize) -> ! {
    let slot = arg as u16;
    kairos::println("echo_server: ready");
    let mut msg = Message::data(0, [0u64; kairos::MSG_WORDS]);
    loop {
        if kairos::recv(slot, &mut msg) != 1 {
            continue;
        }
        kairos::print("echo_server: got #");
        kairos::print_num(msg.tag as u64);
        kairos::print("  payload[0]=");
        kairos::print_num(msg.words[0]);
        kairos::println("");
        if msg.kind == kairos::MsgKind::Data {
            let _ = kairos::send(slot, &msg);
        }
    }
}