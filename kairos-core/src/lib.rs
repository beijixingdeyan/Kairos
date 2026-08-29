//! Kairos microkernel — logic core.
//!
//! `kairos-core` contains everything that is *pure logic* and therefore must
//! be trivially testable on the host, fuzzable, and cross-compilable to any
//! freestanding target (x86_64, RISC-V, ARM). It has **no dependencies**,
//! no I/O and no unsafe code: the architecture rule is that every decision
//! that can be made without touching hardware lives here, so that the kernel
//! itself shrinks to thin, auditable hardware drivers.
//!
//! Modules
//! -------
//! * [`caps`] — capability-based access control (seL4-inspired `CNode`s).
//! * [`sched`] — deterministic scheduler policies (RR / weighted RR / EDF).
//! * [`ipc`]  — bounded-channel message passing core with capability transfer.
//! * [`mem`]  — physical frame allocation (first-fit bitmap allocator).
//! * [`config`] — compile-time configuration baked from `KAIROS_*` variables.
//!
//! Everything in this crate is `#![no_std]` and runs unchanged on the host
//! under `cargo test` and under `proptest`.

#![no_std]
#![deny(unsafe_code)]
#![deny(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod caps;
pub mod config;
pub mod ipc;
pub mod mem;
pub mod sched;

/// Re-export of the config constants as a convenience.
pub use config::*;