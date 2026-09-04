use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lifx::{Color, Power};

pub const COLORS: [(&str, Color); 9] = [
    ("Warm White", Color::white(2_700, 55_000)),
    ("Amber", Color::new(7_500, 50_000, 55_000, 3_500)),
    ("Gold", Color::new(10_500, 48_000, 55_000, 3_500)),
    ("Green", Color::new(21_845, 52_000, 55_000, 3_500)),
    ("Teal", Color::new(27_500, 50_000, 55_000, 3_500)),
    ("Cyan", Color::new(32_768, 50_000, 55_000, 3_500)),
    ("Azure", Color::new(38_229, 52_000, 55_000, 3_500)),
    ("Blue", Color::new(43_690, 52_000, 55_000, 3_500)),
    ("Violet", Color::new(49_151, 48_000, 55_000, 3_500)),
];

#[derive(Clone)]
pub struct TestModeState {
    desired_on: Arc<AtomicBool>,
    color_index: Arc<AtomicUsize>,
}

impl TestModeState {
    pub fn new(initial_power: Power) -> Self {
        Self {
            desired_on: Arc::new(AtomicBool::new(matches!(initial_power, Power::On))),
            color_index: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn power(&self) -> Power {
        if self.desired_on.load(Ordering::Relaxed) {
            Power::On
        } else {
            Power::Off
        }
    }

    pub fn set_power(&self, power: Power) {
        self.desired_on
            .store(matches!(power, Power::On), Ordering::Relaxed);
    }

    /// Return the color currently owned by Test Mode.
    pub fn current_color(&self) -> (&'static str, Color) {
        COLORS[self.color_index.load(Ordering::Relaxed) % COLORS.len()]
    }

    pub fn advance_color(&self) {
        let current = self.color_index.load(Ordering::Relaxed);
        self.color_index
            .store((current + 1) % COLORS.len(), Ordering::Relaxed);
    }
}
