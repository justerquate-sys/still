//! Coordinate conversions. All public points are top-left-origin view coordinates.

use crate::coordinates::{Point as NSPoint, Rect as NSRect, Size as NSSize};

fn finite_rect(rect: NSRect) -> bool {
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && (rect.origin.x + rect.size.width).is_finite()
        && (rect.origin.y + rect.size.height).is_finite()
}

fn normalized(rect: NSRect) -> NSRect {
    NSRect::new(
        NSPoint::new(
            rect.origin.x.min(rect.origin.x + rect.size.width),
            rect.origin.y.min(rect.origin.y + rect.size.height),
        ),
        NSSize::new(rect.size.width.abs(), rect.size.height.abs()),
    )
}

fn intersect(a: NSRect, b: NSRect) -> NSRect {
    let x = a.origin.x.max(b.origin.x);
    let y = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if right <= x || bottom <= y {
        return NSRect::ZERO;
    }
    NSRect::new(NSPoint::new(x, y), NSSize::new(right - x, bottom - y))
}

/// Standardize a drag in any direction and clip it to one display.
pub fn selection(start: NSPoint, end: NSPoint, bounds: NSRect) -> NSRect {
    if !start.x.is_finite()
        || !start.y.is_finite()
        || !end.x.is_finite()
        || !end.y.is_finite()
        || !finite_rect(bounds)
    {
        return NSRect::ZERO;
    }
    let rect = NSRect::new(
        NSPoint::new(start.x.min(end.x), start.y.min(end.y)),
        NSSize::new((end.x - start.x).abs(), (end.y - start.y).abs()),
    );
    intersect(rect, normalized(bounds))
}

/// Convert selection points into outward-rounded original image pixels.
/// A sub-two-point drag is treated as an accidental click.
pub fn pixel_rect(selection: NSRect, bounds: NSRect, image_size: NSSize) -> Option<NSRect> {
    if !finite_rect(selection)
        || !finite_rect(bounds)
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
        || !image_size.width.is_finite()
        || !image_size.height.is_finite()
        || image_size.width <= 0.0
        || image_size.height <= 0.0
    {
        return None;
    }
    let clipped = intersect(normalized(selection), bounds);
    if clipped.size.width < 2.0 || clipped.size.height < 2.0 {
        return None;
    }
    let sx = image_size.width / bounds.size.width;
    let sy = image_size.height / bounds.size.height;
    let x = ((clipped.origin.x - bounds.origin.x) * sx).floor().max(0.0);
    let y = ((clipped.origin.y - bounds.origin.y) * sy).floor().max(0.0);
    let right = ((clipped.origin.x + clipped.size.width - bounds.origin.x) * sx)
        .ceil()
        .min(image_size.width);
    let bottom = ((clipped.origin.y + clipped.size.height - bounds.origin.y) * sy)
        .ceil()
        .min(image_size.height);
    if right <= x || bottom <= y || ![x, y, right, bottom].iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(right - x, bottom - y),
    ))
}

/// Fit (including enlarge) the image into the available drawing area.
pub fn fitted_canvas(image_size: NSSize, area: NSRect) -> NSRect {
    if !finite_rect(area)
        || area.size.width <= 0.0
        || area.size.height <= 0.0
        || !image_size.width.is_finite()
        || !image_size.height.is_finite()
        || image_size.width <= 0.0
        || image_size.height <= 0.0
    {
        return NSRect::ZERO;
    }
    let scale = (area.size.width / image_size.width).min(area.size.height / image_size.height);
    let size = NSSize::new(image_size.width * scale, image_size.height * scale);
    if !scale.is_finite() || !size.width.is_finite() || !size.height.is_finite() {
        return NSRect::ZERO;
    }
    NSRect::new(
        NSPoint::new(
            area.origin.x + (area.size.width - size.width) / 2.0,
            area.origin.y + (area.size.height - size.height) / 2.0,
        ),
        size,
    )
}

/// Map a point on the enlarged canvas back into original pixel coordinates.
/// Points outside the canvas remain outside, allowing clipping instead of edge smears.
pub fn image_point(point: NSPoint, canvas: NSRect, image_size: NSSize) -> NSPoint {
    if !finite_rect(canvas)
        || canvas.size.width <= 0.0
        || canvas.size.height <= 0.0
        || !point.x.is_finite()
        || !point.y.is_finite()
        || !image_size.width.is_finite()
        || !image_size.height.is_finite()
        || image_size.width <= 0.0
        || image_size.height <= 0.0
    {
        return NSPoint::ZERO;
    }
    let result = NSPoint::new(
        (point.x - canvas.origin.x) * image_size.width / canvas.size.width,
        (point.y - canvas.origin.y) * image_size.height / canvas.size.height,
    );
    if result.x.is_finite() && result.y.is_finite() {
        result
    } else {
        NSPoint::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    #[test]
    fn reverse_drag_clips_to_display() {
        assert_eq!(
            selection(
                NSPoint::new(120.0, 85.0),
                NSPoint::new(-20.0, 10.0),
                rect(0.0, 0.0, 100.0, 80.0)
            ),
            rect(0.0, 10.0, 100.0, 70.0)
        );
    }

    #[test]
    fn retina_crop_preserves_top_left_origin() {
        assert_eq!(
            pixel_rect(
                rect(20.0, 10.0, 40.0, 25.0),
                rect(0.0, 0.0, 100.0, 80.0),
                NSSize::new(200.0, 160.0)
            ),
            Some(rect(40.0, 20.0, 80.0, 50.0))
        );
    }

    #[test]
    fn fractional_crop_rounds_outward_with_nonzero_origin() {
        assert_eq!(
            pixel_rect(
                rect(-98.75, 51.25, 7.5, 4.5),
                rect(-100.0, 50.0, 100.0, 80.0),
                NSSize::new(200.0, 160.0)
            ),
            Some(rect(2.0, 2.0, 16.0, 10.0))
        );
    }

    #[test]
    fn rejects_accidental_click_and_nonfinite_geometry() {
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let size = NSSize::new(200.0, 200.0);
        for bad in [
            rect(1.0, 1.0, 1.5, 10.0),
            rect(200.0, 10.0, 10.0, 10.0),
            rect(f64::NAN, 0.0, 10.0, 10.0),
            rect(0.0, 0.0, f64::INFINITY, 10.0),
        ] {
            assert_eq!(pixel_rect(bad, bounds, size), None);
        }
    }

    #[test]
    fn fitted_canvas_enlarges_and_centers_without_distortion() {
        assert_eq!(
            fitted_canvas(NSSize::new(200.0, 100.0), rect(50.0, 80.0, 1000.0, 700.0)),
            rect(50.0, 180.0, 1000.0, 500.0)
        );
    }

    #[test]
    fn canvas_coordinates_round_trip_across_scales() {
        for scale in [0.25, 0.5, 1.0, 2.0, 3.7, 8.0] {
            let canvas = rect(23.0, 67.0, 320.0 * scale, 180.0 * scale);
            for (x, y) in [(0.0, 0.0), (50.0, 100.0), (320.0, 180.0), (-20.0, 200.0)] {
                let mapped = image_point(
                    NSPoint::new(23.0 + x * scale, 67.0 + y * scale),
                    canvas,
                    NSSize::new(320.0, 180.0),
                );
                assert!((mapped.x - x).abs() < 1e-9);
                assert!((mapped.y - y).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn pixel_crops_always_stay_in_image_bounds() {
        for sx in [-200.0, -0.1, 0.0, 20.25, 99.0, 150.0] {
            for sy in [-200.0, 0.0, 20.75, 79.0, 150.0] {
                for ex in [-100.0, 0.0, 50.5, 100.0, 200.0] {
                    let selection = selection(
                        NSPoint::new(sx, sy),
                        NSPoint::new(ex, 100.0),
                        rect(0.0, 0.0, 100.0, 80.0),
                    );
                    if let Some(crop) = pixel_rect(
                        selection,
                        rect(0.0, 0.0, 100.0, 80.0),
                        NSSize::new(150.0, 160.0),
                    ) {
                        assert!(crop.origin.x >= 0.0 && crop.origin.y >= 0.0);
                        assert!(crop.origin.x + crop.size.width <= 150.0);
                        assert!(crop.origin.y + crop.size.height <= 160.0);
                        assert!(crop.size.width > 0.0 && crop.size.height > 0.0);
                    }
                }
            }
        }
    }

    #[test]
    fn finite_inputs_cannot_overflow_into_nonfinite_canvas_geometry() {
        assert_eq!(
            fitted_canvas(
                NSSize::new(f64::MIN_POSITIVE, f64::MIN_POSITIVE),
                rect(0.0, 0.0, 1000.0, 1000.0)
            ),
            NSRect::ZERO
        );
        assert_eq!(
            image_point(
                NSPoint::new(f64::MAX, 1.0),
                rect(-f64::MAX, 0.0, 100.0, 100.0),
                NSSize::new(100.0, 100.0)
            ),
            NSPoint::ZERO
        );
    }
}
