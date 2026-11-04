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
/// during pointer tracking. A mosaic is shared by every effect on one capture.
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
            .ok_or("Empty pixelation area")?;
        let cropped = crate::portable_image::crop(&self.pixels, pixel, bounds)?;
        Self::at(cropped, pixel.origin)
    }
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
