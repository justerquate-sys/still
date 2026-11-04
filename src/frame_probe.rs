//! Opt-in native rendering diagnostics. Measures callback cadence and main
//! thread frame preparation, not photons, mouse hardware, or compositor drops.
use std::time::Instant;

struct Sample {
    display_timestamp: f64,
    arrival: f64,
    work_ms: f64,
}

pub(crate) struct FrameProbe {
    pub started: Instant,
    pub stage: usize,
    samples: [Vec<Sample>; 3],
}

impl FrameProbe {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            stage: 0,
            samples: std::array::from_fn(|_| Vec::with_capacity(512)),
        }
    }

    pub fn record(&mut self, timestamp: f64, started: Instant) {
        let samples = &mut self.samples[self.stage.min(2)];
        if samples.len() < 4096 {
            samples.push(Sample {
                display_timestamp: timestamp,
                arrival: started.duration_since(self.started).as_secs_f64(),
                work_ms: started.elapsed().as_secs_f64() * 1000.,
            });
        }
    }

    pub fn report(&self) {
        for (label, samples) in ["selection", "zoom", "drawing"]
            .into_iter()
            .zip(&self.samples)
        {
            if samples.len() < 2 {
                continue;
            }
            let mut work: Vec<_> = samples.iter().map(|s| s.work_ms).collect();
            let mut gaps: Vec<_> = samples
                .windows(2)
                .map(|s| (s[1].arrival - s[0].arrival) * 1000.)
                .collect();
            let mut display_gaps: Vec<_> = samples
                .windows(2)
                .map(|s| (s[1].display_timestamp - s[0].display_timestamp) * 1000.)
                .collect();
            work.sort_by(f64::total_cmp);
            gaps.sort_by(f64::total_cmp);
            display_gaps.sort_by(f64::total_cmp);
            let span = samples.last().unwrap().arrival - samples[0].arrival;
            println!(
                "RENDER CHECK: {label}: frames={}, callback_hz={:.1}, interval_p50_ms={:.3}, interval_p95_ms={:.3}, display_interval_p95_ms={:.3}, prepare_p50_ms={:.3}, prepare_p95_ms={:.3}",
                samples.len(),
                (samples.len() - 1) as f64 / span,
                percentile(&gaps, 0.5),
                percentile(&gaps, 0.95),
                percentile(&display_gaps, 0.95),
                percentile(&work, 0.5),
                percentile(&work, 0.95)
            );
        }
        println!(
            "RENDER CHECK: complete (callback cadence and CPU preparation only; synthetic content, no general clipboard)"
        );
    }
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    values[((values.len() - 1) as f64 * fraction).ceil() as usize]
}
