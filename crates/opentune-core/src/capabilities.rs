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
    /// Can open a VW TP2.0 channel (KWP2000 to older VAG modules), selected
    /// with the `transport=tp20` option (see
    /// [`tp20channel`](crate::protocol::tp20channel)).
    pub vwtp20: bool,
}
