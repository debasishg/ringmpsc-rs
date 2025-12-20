/// Optional metrics for monitoring channel performance.
#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub batches_sent: u64,
    pub batches_received: u64,
    pub reserve_spins: u64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }
}
