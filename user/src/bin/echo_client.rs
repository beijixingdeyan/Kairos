//! IPC demo — echo client: pings the server and prints the echo round trip.
#![no_std]
#![no_main]

use kairos;
use kairos::Message;

/// # Safety: raw entry point; the kernel passes the channel slot in `rdi`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(arg: usize) -> ! {
    let slot = (arg as u16).wrapping_sub(1);
    kairos::println("echo_client: starting");
    let mut n: u64 = 0;
    loop {
        let mut msg = Message::data(n as u16, [n; kairos::MSG_WORDS]);
        if kairos::send(slot, &msg) != 1 {
            kairos::println("echo_client: send failed (channel full?)");
            continue;
        }
        msg = Message::data(0, [0; kairos::MSG_WORDS]);
        if kairos::recv(slot, &mut msg) == 1 {
            kairos::print("echo_client: round trip #");
            kairos::print_num(n);
            kairos::print(" ok, echo payload=");
            kairos::print_num(msg.words[0]);
            kairos::println("");
        } else {
            kairos::print("echo_client: recv#");
            kairos::print_num(n);
            kairos::println(" failed");
        }
        n += 1;
        kairos::sleep(1000);
    }
}