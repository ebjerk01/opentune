//! Long-running adapter operations shared by the front ends. Each one runs on
//! [`AdapterHub::start_op`](crate::adapter::AdapterHub::start_op), keeps its
//! live state outside the hub (so status and stop never wait on the busy
//! adapter) and ends on a stop flag or its own deadline. The front end keeps
//! only the glue: its commands, its managed state and its session log.
//!
//! * [`candump`] — a full CAN capture to a candump / CSV / ASC file.
//! * [`actuator`] — timed actuator tests (UDS 0x2F) with stop and automatic
//!   return of control.

pub mod actuator;
pub mod candump;
