pub struct AudioTrack {
    samples: Vec<f32>,
    sample_index: usize,
    active: bool,
}

impl AudioTrack {
    pub fn new(samples: Vec<f32>) -> Self {
        Self {
            samples,
            sample_index: 0,
            active: true,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn next_sample(&mut self) -> f32 {
        if !self.active || self.sample_index >= self.samples.len() {
            self.active = false;
            return 0.0;
        }
        let s = self.samples[self.sample_index];
        self.sample_index += 1;
        if self.sample_index >= self.samples.len() {
            self.active = false;
        }
        s
    }
}
