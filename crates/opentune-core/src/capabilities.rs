//! What an interface driver can do. Apps query this to grey out actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub can: bool,
    pub can_fd: bool,
    pub kline: bool,
    pub read_memory: bool,
    pub write_memory: bool,
    /// Bench/boot (BDM) access — OpenTune's OBD drivers do not provide this.
    pub bench_boot: bool,
    pub battery_voltage: bool,
    pub programming_voltage: bool,
}
