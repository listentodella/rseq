#![cfg_attr(not(feature = "std"), no_std)]

pub mod bus;
pub mod opcode;
pub mod vm;

pub use bus::{Bus, BusError, BusKind, ReportArg};
pub use opcode::{Opcode, REPORT_ARG_BYTES, REPORT_ARG_U32};
pub use vm::{DATA_BUF_COUNT, DATA_BUF_LEN, PROBE_MAX_CANDIDATES, Vm, VmError};
