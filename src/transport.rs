pub struct Transport {
    bpm: f64,
    sample_rate: f64,
    pub current_sample: u64,
}

impl Transport {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            bpm: 120.0,
            sample_rate,
            current_sample: 0,
        }
    }

    pub fn samples_per_beat(&self) -> u64 {
        ((self.sample_rate * 60.0) / self.bpm) as u64
    }

    pub fn advance(&mut self, n: usize) {
        self.current_sample += n as u64;
    }

    pub fn reset(&mut self) {
        self.current_sample = 0;
    }
}
