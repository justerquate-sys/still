//! Platform-independent annotation data and geometry.
use crate::coordinates::Point;
use std::sync::Arc;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Arrow,
    Rectangle,
    Highlighter,
    Text,
    PixelateArea,
    PixelateBrush,
}
impl Tool {
    pub const ALL: [Self; 7] = [
        Self::Pen,
        Self::Arrow,
        Self::Rectangle,
        Self::Highlighter,
        Self::Text,
        Self::PixelateArea,
        Self::PixelateBrush,
    ];
    pub fn is_pixelation(self) -> bool {
        matches!(self, Self::PixelateArea | Self::PixelateBrush)
    }
}
/// Immutable, prepared pixels. Native upload formats are created once, never
/// during pointer tracking. A synthetic texture is shared by redactions on one capture.
#[derive(Clone, Debug)]
pub struct Stamp {
    pub pixels: crate::portable_image::Image,
    pub origin: Point,
    #[cfg(target_os = "macos")]
    pub native: crate::imaging::Image,
}
impl Stamp {
    pub fn new(pixels: crate::portable_image::Image) -> Result<Arc<Self>, String> {
        Self::at(pixels, Point::new(0., 0.))
    }
    fn at(pixels: crate::portable_image::Image, origin: Point) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            #[cfg(target_os = "macos")]
            native: crate::imaging::from_rgba(pixels.width(), pixels.height(), pixels.pixels())?,
            pixels,
            origin,
        }))
    }
    pub fn patch(&self, mark: &Mark) -> Result<Arc<Self>, String> {
        let first = mark.points[0];
        let mut min = first;
        let mut max = first;
        for p in &mark.points {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        let padding = if mark.tool == Tool::PixelateBrush {
            mark.width + 2.
        } else {
            2.
        };
        let bounds = crate::coordinates::Rect::new(
            Point::new(0., 0.),
            crate::coordinates::Size::new(self.pixels.width() as f64, self.pixels.height() as f64),
        );
        let requested = crate::coordinates::Rect::new(
            Point::new(min.x - padding, min.y - padding),
            crate::coordinates::Size::new(
                max.x - min.x + padding * 2.,
                max.y - min.y + padding * 2.,
            ),
        );
        let pixel = crate::geometry::pixel_rect(requested, bounds, bounds.size)
            .ok_or("Empty redaction area")?;
        let cropped = crate::portable_image::crop(&self.pixels, pixel, bounds)?;
        Self::at(cropped, pixel.origin)
    }
}

/// A small collar strictly outside the rounded-out exported crop. Automatic
/// redaction tone may read these regions, never pixels within any exported area.
/// Empty collars (including a full-screen crop) deliberately yield no samples.
pub(crate) fn redaction_sample_regions(
    width: usize,
    height: usize,
    crop: crate::coordinates::Rect,
) -> [crate::coordinates::Rect; 4] {
    use crate::coordinates::{Rect, Size};
    let width = width as f64;
    let height = height as f64;
    let left = crop.origin.x.floor().clamp(0., width);
    let top = crop.origin.y.floor().clamp(0., height);
    let right = (crop.origin.x + crop.size.width).ceil().clamp(left, width);
    let bottom = (crop.origin.y + crop.size.height).ceil().clamp(top, height);
    let collar = 8.;
    [
        Rect::new(
            Point::new(left, (top - collar).max(0.)),
            Size::new(right - left, top.min(collar)),
        ),
        Rect::new(
            Point::new(left, bottom),
            Size::new(right - left, (height - bottom).min(collar)),
        ),
        Rect::new(
            Point::new((left - collar).max(0.), top),
            Size::new(left.min(collar), bottom - top),
        ),
        Rect::new(
            Point::new(right, top),
            Size::new((width - right).min(collar), bottom - top),
        ),
    ]
}
#[derive(Clone, Debug)]
pub struct Mark {
    pub tool: Tool,
    pub points: Vec<Point>,
    pub color: [f64; 4],
    pub width: f64,
    pub stamp: Option<Arc<Stamp>>,
}
impl Mark {
    pub fn valid(&self) -> bool {
        !self.points.is_empty()
            && self.width.is_finite()
            && self.width > 0.
            && self.color.iter().all(|c| c.is_finite())
            && self
                .points
                .iter()
                .all(|p| p.x.is_finite() && p.y.is_finite())
    }
    pub fn arrow_head(&self) -> Option<[Point; 3]> {
        let (first, last) = (self.points.first()?, self.points.last()?);
        let length = (last.x - first.x).hypot(last.y - first.y);
        if !self.valid() || length <= 1. {
            return None;
        }
        let angle = (last.y - first.y).atan2(last.x - first.x);
        let head = (self.width * 4.5).max(12.).min(length * 0.45);
        let spread = std::f64::consts::PI / 6.;
        Some([
            Point::new(
                last.x - head * (angle - spread).cos(),
                last.y - head * (angle - spread).sin(),
            ),
            *last,
            Point::new(
                last.x - head * (angle + spread).cos(),
                last.y - head * (angle + spread).sin(),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinates::{Rect, Size};

    #[test]
    fn automatic_tone_collars_exclude_every_rounded_export_pixel() {
        let bounds = Rect::new(Point::new(7., 11.), Size::new(40., 30.));
        for selection in [
            bounds,
            Rect::new(Point::new(9.125, 15.375), Size::new(18.2, 9.4)),
            Rect::new(Point::new(7., 11.), Size::new(8.5, 7.25)),
            Rect::new(Point::new(32.75, 33.25), Size::new(14.25, 7.75)),
        ] {
            let crop = crate::geometry::pixel_rect(selection, bounds, Size::new(80., 60.)).unwrap();
            let regions = redaction_sample_regions(80, 60, crop);
            for region in regions {
                for y in region.origin.y as usize..(region.origin.y + region.size.height) as usize {
                    for x in
                        region.origin.x as usize..(region.origin.x + region.size.width) as usize
                    {
                        assert!(x < 80 && y < 60);
                        assert!(
                            (x as f64) < crop.origin.x
                                || (x as f64) >= crop.origin.x + crop.size.width
                                || (y as f64) < crop.origin.y
                                || (y as f64) >= crop.origin.y + crop.size.height
                        );
                    }
                }
            }
            if selection == bounds {
                assert!(regions.iter().all(|r| r.size.width * r.size.height == 0.));
            }
        }
    }
}
