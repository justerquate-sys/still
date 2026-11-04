//! One refresh policy for both platforms. Never invent frames a display cannot show.
use std::time::{Duration, Instant};
pub const TARGET_HZ: f64 = 240.;
pub const ZOOM_SECONDS: f64 = 0.18;
pub fn target_hz(display_hz: f64) -> f64 {
    if display_hz.is_finite() && display_hz >= 24. {
        display_hz.min(TARGET_HZ)
    } else {
        60.
    }
}
pub fn frame_interval(display_hz: f64) -> Duration {
    Duration::from_secs_f64(1. / target_hz(display_hz))
}
/// Keep timer jitter from accumulating into a lower frame rate. Skip missed
/// frames after a stall; never issue a burst of old pointer positions.
pub fn next_frame_deadline(previous: Instant, now: Instant, display_hz: f64) -> Instant {
    let interval = frame_interval(display_hz);
    let next = previous + interval;
    if next > now { next } else { now + interval }
}
pub fn ease_out(t: f64) -> f64 {
    if t <= 0. {
        return 0.;
    }
    if t >= 1. {
        return 1.;
    }
    let (mut low, mut high) = (0., 1.);
    for _ in 0..16 {
        let u = (low + high) * 0.5;
        let v = 1. - u;
        if 3. * v * v * u * 0.22 + 3. * v * u * u * 0.36 + u * u * u < t {
            low = u;
        } else {
            high = u;
        }
    }
    1. - (1. - (low + high) * 0.5).powi(3)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_follow_hardware_up_to_240() {
        for hz in [60., 120., 144., 165., 239.76, 240.] {
            assert_eq!(target_hz(hz), hz);
        }
        assert_eq!(target_hz(360.), 240.);
        for hz in [0., -1., f64::NAN, f64::INFINITY] {
            assert_eq!(target_hz(hz), 60.);
        }
        assert!((frame_interval(240.).as_secs_f64() - 1. / 240.).abs() < 1e-9);
    }
    #[test]
    fn late_callbacks_keep_cadence_and_long_stalls_skip_old_frames() {
        let origin = Instant::now();
        let period = frame_interval(240.);
        let mut deadline = origin;
        for frame in 1..=1000 {
            // A stable 0.2 ms timer delay must not add 0.2 ms to every period.
            deadline = next_frame_deadline(deadline, deadline + Duration::from_micros(200), 240.);
            assert_eq!(deadline.duration_since(origin), period * frame);
        }
        let after_stall = deadline + Duration::from_millis(80);
        assert_eq!(
            next_frame_deadline(deadline, after_stall, 240.),
            after_stall + period
        );
    }
    #[test]
    fn easing_is_bounded_monotonic() {
        let mut previous = 0.;
        for i in 0..=1000 {
            let v = ease_out(i as f64 / 1000.);
            assert!(v >= previous && v <= 1.);
            previous = v;
        }
        assert_eq!(ease_out(0.), 0.);
        assert_eq!(ease_out(1.), 1.);
    }
}
