use std::f64::consts::TAU;

pub struct Voice {
    active: bool,
    releasing: bool,
    note: u8,
    phase: f64,
    freq: f64,
    sample_rate: f64,
    env_gain: f64,
}

impl Voice {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            active: false,
            releasing: false,
            note: 0,
            phase: 0.0,
            freq: 440.0,
            sample_rate,
            env_gain: 0.0,
        }
    }

    pub fn note_on(&mut self, note: u8, freq: f64) {
        self.active = true;
        self.releasing = false;
        self.note = note;
        self.freq = freq;
        self.env_gain = 1.0;
    }

    pub fn note_off(&mut self) {
        self.releasing = true;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn note(&self) -> u8 {
        self.note
    }

    pub fn next_sample(&mut self) -> f32 {
        if !self.active {
            return 0.0;
        }

        let sample = ((TAU * self.phase).sin() * self.env_gain) as f32;

        self.phase += self.freq / self.sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        const RELEASE_RATE: f64 = 0.001;
        if self.releasing {
            self.env_gain -= RELEASE_RATE;
            if self.env_gain <= 0.0 {
                self.env_gain = 0.0;
                self.active = false;
                self.releasing = false;
            }
        }

        sample
    }
}
