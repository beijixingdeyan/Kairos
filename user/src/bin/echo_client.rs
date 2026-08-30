//! IPC demo — echo client: pings the server and prints the echo round trip.
#![no_std]
#![no_main]

use kairos;
use kairos::Message;

/// Consecutive send failures that make us give up. A healthy echo session
/// never fails a send (the server drains as fast as we produce); only a
/// missing/broken channel (e.g. spawned without `ipcdemo`) fails
/// repeatedly, so a bounded cap turns that into a clean diagnostic exit
/// instead of an infinite retry flood.
const MAX_CONSECUTIVE_FAILS: u64 = 16;

/// # Safety: raw entry point; the kernel passes the channel slot in `rdi`
/// (1-based capability slot).
#[unsafe(no_mangle)]
pub extern "C" fn _start(arg: usize) -> ! {
    let slot = arg as u16;
    kairos::println("echo_client: starting");
    let mut n: u64 = 0;
    let mut fails: u64 = 0;
    loop {
        let mut msg = Message::data(n as u16, [n; kairos::MSG_WORDS]);
        if kairos::send(slot, &msg) != 1 {
            fails += 1;
            if fails >= MAX_CONSECUTIVE_FAILS {
                kairos::println(
                    "echo_client: no working channel (use the shell's `ipcdemo`); exiting",
                );
                kairos::exit(0);
            }
            // Back off so a temporarily full channel can drain: blocking
            // sleeps give the server (and everyone else) CPU time.
            kairos::sleep(100);
            continue;
        }
        fails = 0;
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