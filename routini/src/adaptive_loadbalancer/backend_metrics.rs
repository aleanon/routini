use std::sync::atomic::{AtomicUsize, Ordering};

use atomic_float::AtomicF64;

pub struct BackendMetrics {
    pub avg_latency: AtomicF64,
    pub alpha: f64,
    pub connection_count: AtomicUsize,
}

impl BackendMetrics {
    pub fn new(smoothing_factor: f64) -> Self {
        Self {
            avg_latency: AtomicF64::new(0.0),
            alpha: smoothing_factor,
            connection_count: AtomicUsize::new(0),
        }
    }

    pub fn record_latency(&self, latency: u32) {
        let latency = latency as f64;

        let avg = self.avg_latency.load(Ordering::Relaxed);
        if avg == 0.0 {
            self.avg_latency.store(latency, Ordering::Relaxed);
        } else {
            let latency = self.alpha * latency + (1.0 - self.alpha) * avg;
            self.avg_latency.store(latency, Ordering::Relaxed);
        }
    }

    pub fn increment_connection_count(&self) {
        self.connection_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_connection_count(&self) {
        self.connection_count.fetch_sub(1, Ordering::Relaxed);
    }
}
