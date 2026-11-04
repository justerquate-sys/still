//! Keep captured pixels in Core Animation instead of rasterizing the whole
//! Retina desktop for each pointer update. This view never intercepts input.
use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSView};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransform3D};

use crate::{imaging::Image, ui};

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct BackdropView;
    unsafe impl NSObjectProtocol for BackdropView {}
    impl BackdropView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _: NSPoint) -> *mut NSView { std::ptr::null_mut() }
    }
);

pub(crate) struct Compositor {
    view: Retained<BackdropView>,

    desktop: Retained<CALayer>,
    dim: [Retained<CALayer>; 4],
    crop: Retained<CALayer>,
    crop_identity: Cell<*const CGImage>,
}

impl Compositor {
    pub fn new(mtm: MainThreadMarker, image: &Image, bounds: NSRect) -> Self {
        // SAFETY: NSView's designated initializer on our allocated subclass.
        let view: Retained<BackdropView> =
            unsafe { msg_send![BackdropView::alloc(mtm), initWithFrame: bounds] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let root = CALayer::new();
        root.setGeometryFlipped(true);
        root.setFrame(bounds);
        root.setBackgroundColor(Some(&ui::color(ui::DEEP).CGColor()));
        let desktop = CALayer::new();
        set_image(&desktop, image);
        desktop.setFrame(bounds);
        root.addSublayer(&desktop);
        let shade = NSColor::colorWithSRGBRed_green_blue_alpha(0., 0., 0., 0.48).CGColor();
        let dim = std::array::from_fn(|_| {
            let layer = CALayer::new();
            layer.setBackgroundColor(Some(&shade));
            root.addSublayer(&layer);
            layer
        });
        let crop = CALayer::new();
        crop.setHidden(true);
        crop.setAnchorPoint(NSPoint::ZERO);
        crop.setShadowColor(Some(&NSColor::blackColor().CGColor()));
        crop.setShadowOpacity(0.3);
        crop.setShadowRadius(12.);
        crop.setShadowOffset(NSSize::new(0., 8.));
        root.addSublayer(&crop);
        // Setting the custom layer first makes this a layer-hosting view.
        view.setLayer(Some(&root));
        view.setWantsLayer(true);
        let this = Self {
            view,
            desktop,
            dim,
            crop,
            crop_identity: Cell::new(std::ptr::null()),
        };
        this.update(bounds, NSRect::ZERO, NSRect::ZERO, false, None);
        this
    }

    pub fn view(&self) -> &NSView {
        &self.view
    }

    /// Caller disables implicit animations in the enclosing transaction: every
    /// pointer position is shown directly, with no trailing interpolation.
    pub fn update(
        &self,
        bounds: NSRect,
        selection: NSRect,
        canvas: NSRect,
        editing: bool,
        image: Option<&Image>,
    ) {
        self.desktop.setFrame(bounds);
        self.desktop.setHidden(editing);
        self.crop.setHidden(!editing);
        for (layer, frame) in self.dim.iter().zip(dim_regions(bounds, selection)) {
            layer.setHidden(editing);
            layer.setFrame(frame);
        }
        if editing {
            if let Some(image) = image {
                let identity = image.cg_image() as *const CGImage;
                if self.crop_identity.replace(identity) != identity {
                    set_image(&self.crop, image);
                }
            }
            if canvas.size.width > 0. && canvas.size.height > 0. {
                let target_bounds = NSRect::new(NSPoint::ZERO, canvas.size);
                if self.crop.bounds() != target_bounds {
                    self.crop.setBounds(target_bounds);
                    self.crop.setShadowPath(Some(
                        &NSBezierPath::bezierPathWithRect(target_bounds).CGPath(),
                    ));
                }
                // The crop has stable backing dimensions. Zoom only changes its
                // compositor transform/position, never view layout or shadow masks.
                self.crop.setPosition(selection.origin);
                self.crop.setTransform(CATransform3D::new_scale(
                    selection.size.width / canvas.size.width,
                    selection.size.height / canvas.size.height,
                    1.,
                ));
            }
        }
    }
}

fn set_image(layer: &CALayer, image: &Image) {
    // SAFETY: CALayer.contents explicitly accepts CGImage. Core Animation
    // retains the immutable image; no borrowed pixel buffer crosses a frame.
    unsafe {
        layer.setContents(Some(
            &*(image.cg_image() as *const CGImage).cast::<AnyObject>(),
        ));
    }
}

/// Four disjoint rectangles leave the selected region clear. Unlike overlapping
/// dimming layers, each outside pixel receives the shade exactly once.
fn dim_regions(bounds: NSRect, selection: NSRect) -> [NSRect; 4] {
    if selection.size.width <= 1.0 || selection.size.height <= 1.0 {
        return [bounds, NSRect::ZERO, NSRect::ZERO, NSRect::ZERO];
    }
    let left = selection
        .origin
        .x
        .clamp(bounds.origin.x, bounds.origin.x + bounds.size.width);
    let top = selection
        .origin
        .y
        .clamp(bounds.origin.y, bounds.origin.y + bounds.size.height);
    let right = (selection.origin.x + selection.size.width)
        .clamp(left, bounds.origin.x + bounds.size.width);
    let bottom = (selection.origin.y + selection.size.height)
        .clamp(top, bounds.origin.y + bounds.size.height);
    [
        NSRect::new(
            bounds.origin,
            NSSize::new(bounds.size.width, top - bounds.origin.y),
        ),
        NSRect::new(
            NSPoint::new(bounds.origin.x, bottom),
            NSSize::new(
                bounds.size.width,
                (bounds.origin.y + bounds.size.height) - bottom,
            ),
        ),
        NSRect::new(
            NSPoint::new(bounds.origin.x, top),
            NSSize::new(left - bounds.origin.x, bottom - top),
        ),
        NSRect::new(
            NSPoint::new(right, top),
            NSSize::new((bounds.origin.x + bounds.size.width) - right, bottom - top),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimming_has_no_gaps_or_double_darkening() {
        let bounds = NSRect::new(NSPoint::ZERO, NSSize::new(12., 8.));
        for selection in [
            NSRect::ZERO,
            bounds,
            NSRect::new(NSPoint::new(2., 3.), NSSize::new(7., 4.)),
        ] {
            let regions = dim_regions(bounds, selection);
            for x in 0..12 {
                for y in 0..8 {
                    let point = NSPoint::new(x as f64 + 0.5, y as f64 + 0.5);
                    let contains = |r: NSRect| {
                        point.x >= r.origin.x
                            && point.x < (r.origin.x + r.size.width)
                            && point.y >= r.origin.y
                            && point.y < (r.origin.y + r.size.height)
                    };
                    let hits = regions.iter().filter(|&&r| contains(r)).count();
                    assert_eq!(hits, usize::from(!contains(selection)));
                }
            }
        }
    }
}
