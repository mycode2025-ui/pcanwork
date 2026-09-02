#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimestampQualitySnapshot {
    pub samples: u64,
    pub latest_transport_jitter_us: f64,
    pub max_transport_jitter_us: f64,
    pub clock_drift_ppm: f64,
    pub monotonic_violations: u64,
}

#[derive(Clone, Debug, Default)]
pub struct TimestampQuality {
    first_device_s: Option<f64>,
    first_host_s: Option<f64>,
    previous_device_s: Option<f64>,
    minimum_latency_s: Option<f64>,
    latest_transport_jitter_s: f64,
    max_transport_jitter_s: f64,
    clock_drift_ppm: f64,
    samples: u64,
    monotonic_violations: u64,
}

impl TimestampQuality {
    /// Start a new hardware connection session without discarding lifetime
    /// sample/error totals or the maximum jitter observed in completed sessions.
    pub fn begin_session(&mut self) {
        self.first_device_s = None;
        self.first_host_s = None;
        self.previous_device_s = None;
        self.minimum_latency_s = None;
        self.latest_transport_jitter_s = 0.0;
        self.clock_drift_ppm = 0.0;
    }

    pub fn observe(&mut self, device_s: f64, host_receive_s: f64) {
        if !device_s.is_finite() || !host_receive_s.is_finite() {
            return;
        }
        if self
            .previous_device_s
            .is_some_and(|previous| device_s < previous)
        {
            self.monotonic_violations += 1;
        }
        self.previous_device_s = Some(device_s);

        let latency = host_receive_s - device_s;
        let minimum = self
            .minimum_latency_s
            .map_or(latency, |current| current.min(latency));
        self.minimum_latency_s = Some(minimum);
        self.latest_transport_jitter_s = (latency - minimum).max(0.0);
        self.max_transport_jitter_s = self
            .max_transport_jitter_s
            .max(self.latest_transport_jitter_s);

        match (self.first_device_s, self.first_host_s) {
            (Some(first_device), Some(first_host)) => {
                let device_span = device_s - first_device;
                let host_span = host_receive_s - first_host;
                if device_span > 1.0 {
                    self.clock_drift_ppm = ((host_span - device_span) / device_span) * 1_000_000.0;
                }
            }
            _ => {
                self.first_device_s = Some(device_s);
                self.first_host_s = Some(host_receive_s);
            }
        }
        self.samples += 1;
    }

    pub fn snapshot(&self) -> TimestampQualitySnapshot {
        TimestampQualitySnapshot {
            samples: self.samples,
            latest_transport_jitter_us: self.latest_transport_jitter_s * 1_000_000.0,
            max_transport_jitter_us: self.max_transport_jitter_s * 1_000_000.0,
            clock_drift_ppm: self.clock_drift_ppm,
            monotonic_violations: self.monotonic_violations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_quality_quantifies_jitter_drift_and_monotonicity() {
        let mut quality = TimestampQuality::default();
        quality.observe(10.0, 10.002);
        quality.observe(20.0, 20.003);
        quality.observe(30.0, 30.004);
        let snapshot = quality.snapshot();
        assert_eq!(snapshot.samples, 3);
        assert!((snapshot.latest_transport_jitter_us - 2000.0).abs() < 0.01);
        assert!((snapshot.max_transport_jitter_us - 2000.0).abs() < 0.01);
        assert!((snapshot.clock_drift_ppm - 100.0).abs() < 0.01);
        assert_eq!(snapshot.monotonic_violations, 0);

        quality.observe(29.0, 31.0);
        assert_eq!(quality.snapshot().monotonic_violations, 1);
    }

    #[test]
    fn reconnect_starts_a_new_timing_baseline_without_fake_jitter() {
        let mut quality = TimestampQuality::default();
        quality.observe(10.0, 10.002);
        quality.observe(20.0, 20.003);
        let before = quality.snapshot();

        quality.begin_session();
        quality.observe(0.1, 125.0);
        let after = quality.snapshot();

        assert_eq!(after.samples, 3);
        assert_eq!(after.monotonic_violations, 0);
        assert_eq!(after.latest_transport_jitter_us, 0.0);
        assert_eq!(
            after.max_transport_jitter_us,
            before.max_transport_jitter_us
        );
        assert_eq!(after.clock_drift_ppm, 0.0);
    }
}
