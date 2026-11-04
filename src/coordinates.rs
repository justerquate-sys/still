//! Shared top-left coordinates; preserve AppKit's native ABI on macOS.
#[cfg(target_os = "macos")]
pub use objc2_foundation::{NSPoint as Point, NSRect as Rect, NSSize as Size};

#[cfg(not(target_os = "macos"))]
mod portable {
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Point {
        pub x: f64,
        pub y: f64,
    }
    impl Point {
        pub const ZERO: Self = Self::new(0., 0.);
        pub const fn new(x: f64, y: f64) -> Self {
            Self { x, y }
        }
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Size {
        pub width: f64,
        pub height: f64,
    }
    impl Size {
        pub const fn new(width: f64, height: f64) -> Self {
            Self { width, height }
        }
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Rect {
        pub origin: Point,
        pub size: Size,
    }
    impl Rect {
        pub const ZERO: Self = Self::new(Point::ZERO, Size::new(0., 0.));
        pub const fn new(origin: Point, size: Size) -> Self {
            Self { origin, size }
        }
    }
}
#[cfg(not(target_os = "macos"))]
pub use portable::*;
