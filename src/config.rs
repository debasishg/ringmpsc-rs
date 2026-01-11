/// Configuration for Ring and Channel.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Ring buffer size as power of 2 (default: 16 = 64K slots)
    pub ring_bits: u8,
    /// Maximum number of producers
    pub max_producers: usize,
    /// Enable metrics collection (slight overhead)
    pub enable_metrics: bool,
}

impl Config {
    /// Creates a new configuration with custom settings.
    /// 
    /// # Panics
    /// 
    /// Panics if `ring_bits` is greater than 20 (1M slots max) to prevent excessive memory usage.
    /// Panics if `max_producers` is 0 or greater than 128.
    pub const fn new(ring_bits: u8, max_producers: usize, enable_metrics: bool) -> Self {
        assert!(ring_bits > 0 && ring_bits <= 20, "ring_bits must be between 1 and 20 (max 1M slots)");
        assert!(max_producers > 0 && max_producers <= 128, "max_producers must be between 1 and 128");
        
        Self {
            ring_bits,
            max_producers,
            enable_metrics,
        }
    }

    /// Returns the capacity of the ring buffer.
    #[inline]
    pub const fn capacity(&self) -> usize {
        1 << self.ring_bits
    }

    /// Returns the mask for index wrapping.
    #[inline]
    pub const fn mask(&self) -> usize {
        self.capacity() - 1
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ring_bits: 16, // 64K slots
            max_producers: 16,
            enable_metrics: false,
        }
    }
}

/// Low latency configuration (4K slots, fits in L1 cache)
pub const LOW_LATENCY_CONFIG: Config = Config::new(12, 16, false);

/// High throughput configuration (256K slots, 32 max producers)
pub const HIGH_THROUGHPUT_CONFIG: Config = Config::new(18, 32, false);
