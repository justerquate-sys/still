//! Still's shared native design: a graphite work surface, lime actions, and
//! paired arrow keycaps. Adapted using unslop by Damir Sokolovsky (@heydamir),
//! https://heydamir.design/unslop — CC BY 4.0.
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::*;
use objc2_foundation::{NSDictionary, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CAAnimation, CABasicAnimation, CAFrameRateRange, CAMediaTiming, CAMediaTimingFunction,
    CATransaction, CATransition,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

pub const BACKGROUND: u32 = 0x151617;
pub const DEEP: u32 = 0x111213;
pub const SURFACE: u32 = 0x1f2022;
pub const RAISED: u32 = 0x2b2d30;
pub const TEXT: u32 = 0xeeefec;
pub const SECONDARY: u32 = 0xb0b2b5;
pub const MUTED: u32 = 0x92959a;
pub const ACCENT: u32 = 0xdbf4a0;
pub const INK: u32 = 0x1b2115;
pub const CORAL: u32 = 0xff756b;
pub const ERROR: u32 = 0xf1a097;
pub const FEEDBACK: f64 = 0.12;
pub const ZOOM: f64 = crate::refresh::ZOOM_SECONDS;
pub const NOTIFICATION: f64 = 0.16;

pub fn status_image() -> Retained<NSImage> {
    let draw = block2::RcBlock::new(|_: NSRect| {
        let path = NSBezierPath::bezierPath();
        path.setLineWidth(1.5);
        path.setLineCapStyle(NSLineCapStyle::Round);
        for (x, y, dx, dy) in [
            (3., 3., 1., 1.),
            (15., 3., -1., 1.),
            (3., 15., 1., -1.),
            (15., 15., -1., -1.),
        ] {
            path.moveToPoint(NSPoint::new(x + dx * 4., y));
            path.lineToPoint(NSPoint::new(x, y));
            path.lineToPoint(NSPoint::new(x, y + dy * 4.));
        }
        for x in [8., 10.] {
            path.moveToPoint(NSPoint::new(x, 7.));
            path.lineToPoint(NSPoint::new(x, 11.));
        }
        NSColor::blackColor().setStroke();
        path.stroke();
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(18., 18.), true, &draw);
    image.setTemplate(true);
    image
}
pub fn pixel_brush_cursor(_mtm: MainThreadMarker) -> Retained<NSCursor> {
    let draw = block2::RcBlock::new(|_: NSRect| {
        let path = NSBezierPath::bezierPathWithOvalInRect(rect(2., 2., 24., 24.));
        NSColor::blackColor().setStroke();
        path.setLineWidth(3.);
        path.stroke();
        NSColor::whiteColor().setStroke();
        path.setLineWidth(1.);
        path.stroke();
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(28., 28.), true, &draw);
    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, NSPoint::new(14., 14.))
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}
pub fn color(hex: u32) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        ((hex >> 16) & 255) as f64 / 255.,
        ((hex >> 8) & 255) as f64 / 255.,
        (hex & 255) as f64 / 255.,
        1.,
    )
}
pub const fn rgba(hex: u32) -> [f64; 4] {
    [
        ((hex >> 16) & 255) as f64 / 255.,
        ((hex >> 8) & 255) as f64 / 255.,
        (hex & 255) as f64 / 255.,
        1.,
    ]
}
pub fn fill(r: NSRect, radius: f64, ink: u32) {
    color(ink).setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius).fill();
}
pub fn outline(r: NSRect, radius: f64, ink: u32, alpha: f64) {
    color(ink).colorWithAlphaComponent(alpha).setStroke();
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
    path.setLineWidth(1.);
    path.stroke();
}
pub fn panel(r: NSRect, radius: f64) {
    fill(r, radius, SURFACE);
    outline(
        rect(
            r.origin.x + 0.5,
            r.origin.y + 0.5,
            r.size.width - 1.,
            r.size.height - 1.,
        ),
        radius - 0.5,
        TEXT,
        0.09,
    );
}
pub fn separator(x: f64, y: f64, width: f64) {
    color(TEXT).colorWithAlphaComponent(0.08).setFill();
    NSBezierPath::fillRect(rect(x, y, width, 1.));
}
pub fn attributes(
    size: f64,
    medium: bool,
    ink: u32,
    mono: bool,
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let weight = if medium { 0.23 } else { 0. };
    let font = if mono {
        NSFont::monospacedSystemFontOfSize_weight(size, weight)
    } else {
        NSFont::systemFontOfSize_weight(size, weight)
    };
    let tint = color(ink);
    // SAFETY: Values have the documented types for the immutable AppKit keys.
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*tint as &AnyObject],
        )
    }
}
pub fn text(value: &str, x: f64, y: f64, size: f64, medium: bool, ink: u32) {
    unsafe {
        NSString::from_str(value).drawAtPoint_withAttributes(
            NSPoint::new(x, y),
            Some(&attributes(size, medium, ink, false)),
        );
    }
}
pub fn centered(value: &str, r: NSRect, size: f64, medium: bool, ink: u32, mono: bool) {
    let value = NSString::from_str(value);
    let attrs = attributes(size, medium, ink, mono);
    unsafe {
        let s = value.sizeWithAttributes(Some(&attrs));
        value.drawAtPoint_withAttributes(
            NSPoint::new(
                r.origin.x + (r.size.width - s.width) / 2.,
                r.origin.y + (r.size.height - s.height) / 2.,
            ),
            Some(&attrs),
        );
    }
}
pub fn keycap(value: &str, r: NSRect, accent: bool) {
    fill(r, 6., if accent { ACCENT } else { RAISED });
    outline(
        rect(
            r.origin.x + 0.5,
            r.origin.y + 0.5,
            r.size.width - 1.,
            r.size.height - 1.,
        ),
        5.5,
        TEXT,
        0.1,
    );
    centered(value, r, 12., true, if accent { INK } else { TEXT }, false);
}

thread_local! { static SYMBOLS: RefCell<HashMap<(&'static str,u32),Retained<NSImage>>> = RefCell::new(HashMap::new()); }
pub fn symbol(name: &'static str, r: NSRect, ink: u32) {
    let image = SYMBOLS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(image) = cache.get(&(name, ink)) {
            return Some(image.clone());
        }
        let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(name),
            None,
        )?;
        let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(16., 0.);
        let tint = NSImageSymbolConfiguration::configurationWithHierarchicalColor(&color(ink));
        let config = config.configurationByApplyingConfiguration(&tint);
        let image = base.imageWithSymbolConfiguration(&config)?;
        cache.insert((name, ink), image.clone());
        Some(image)
    });
    if let Some(image) = image {
        unsafe {
            image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
                r,
                NSRect::ZERO,
                NSCompositingOperation::SourceOver,
                1.,
                true,
                None,
            );
        }
    }
}

/// The original flat crop/pause mark, retained as Still's identity.
pub fn logo(r: NSRect) {
    fill(r, r.size.width * 0.24, ACCENT);
    let p = NSBezierPath::bezierPath();
    let n = r.size.width;
    p.setLineWidth(n * 0.048);
    p.setLineCapStyle(NSLineCapStyle::Round);
    for (x, y, dx, dy) in [
        (0.26, 0.26, 1., 1.),
        (0.74, 0.26, -1., 1.),
        (0.26, 0.74, 1., -1.),
        (0.74, 0.74, -1., -1.),
    ] {
        let x = r.origin.x + n * x;
        let y = r.origin.y + n * y;
        p.moveToPoint(NSPoint::new(x + dx * n * 0.13, y));
        p.lineToPoint(NSPoint::new(x, y));
        p.lineToPoint(NSPoint::new(x, y + dy * n * 0.13));
    }
    for x in [0.46, 0.54] {
        p.moveToPoint(NSPoint::new(r.origin.x + n * x, r.origin.y + n * 0.42));
        p.lineToPoint(NSPoint::new(r.origin.x + n * x, r.origin.y + n * 0.58));
    }
    color(INK).setStroke();
    p.stroke();
}

pub fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}
fn configure_animation(animation: &CAAnimation, view: &NSView, duration: f64) {
    animation.setDuration(duration);
    animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
        0.22, 1., 0.36, 1.,
    )));
    if let Some(screen) = view.window().and_then(|w| w.screen()) {
        let hz = crate::refresh::target_hz(screen.maximumFramesPerSecond() as f64) as f32;
        if hz > 0. {
            animation.setPreferredFrameRateRange(CAFrameRateRange::new(hz, hz, hz));
        }
    }
}
fn feedback(view: &NSView) {
    if !reduce_motion()
        && let Some(layer) = view.layer()
    {
        let transition = CATransition::new();
        configure_animation(&transition, view, FEEDBACK);
        transition.setType(&NSString::from_str("fade"));
        layer.addAnimation_forKey(&transition, Some(&NSString::from_str("still.feedback")));
    }
    view.setNeedsDisplay(true);
}
pub fn fade(view: &NSView, from: f32, to: f32) {
    if let Some(layer) = view.layer() {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setOpacity(to);
        CATransaction::commit();
        if !reduce_motion() {
            let animation =
                CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
            configure_animation(&animation, view, NOTIFICATION);
            // SAFETY: Opacity is a scalar-valued layer property.
            unsafe {
                animation.setFromValue(Some(&NSNumber::new_f32(from)));
                animation.setToValue(Some(&NSNumber::new_f32(to)));
            }
            layer.addAnimation_forKey(&animation, Some(&NSString::from_str("still.visibility")));
        }
    }
}
/// Evaluate the same ease-out as Core Animation by inverting its x coordinate.
pub use crate::refresh::ease_out;

#[derive(Clone, Copy)]
pub enum ButtonStyle {
    Action,
    Primary,
    Quiet,
    Tool,
    Swatch([f64; 4]),
}
pub struct ButtonIvars {
    style: ButtonStyle,
    symbol: Option<&'static str>,
    subtitle: &'static str,
    key: RefCell<String>,
    hovered: Cell<bool>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
}
define_class!(
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ButtonIvars]
    pub struct Button;
    unsafe impl NSObjectProtocol for Button {}
    impl Button {
        #[unsafe(method(isFlipped))] fn is_flipped(&self)->bool {true}
        #[unsafe(method(acceptsFirstResponder))] fn accepts_first_responder(&self)->bool {self.isEnabled()&&!self.isHidden()}
        #[unsafe(method(resetCursorRects))] fn reset_cursor_rects(&self) {self.addCursorRect_cursor(self.bounds(),&NSCursor::arrowCursor());}
        #[unsafe(method(keyDown:))] fn key_down(&self,event:&NSEvent) {
            if event.keyCode()==49 {
                if self.isEnabled()&&!event.isARepeat(){
                    // SAFETY: connect's contract guarantees a live action target.
                    unsafe{self.performClick(None);}
                }
            } else {
                // SAFETY: Preserve NSButton handling for every other key.
                unsafe{let _:()=msg_send![super(self),keyDown:event];}
            }
        }
        #[unsafe(method(drawRect:))] fn draw_rect(&self,_: NSRect) {self.draw_content();}
        #[unsafe(method(focusRingMaskBounds))] fn focus_bounds(&self)->NSRect {self.bounds()}
        #[unsafe(method(drawFocusRingMask))] fn focus_mask(&self) {
            let radius=if matches!(self.ivars().style,ButtonStyle::Action){12.}else{8.};
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(self.bounds(),radius,radius).fill();
        }
        #[unsafe(method(updateTrackingAreas))] fn update_tracking(&self) {
            unsafe { let _:()=msg_send![super(self),updateTrackingAreas]; }
            if let Some(old)=self.ivars().tracking.borrow_mut().take() {self.removeTrackingArea(&old);}
            // SAFETY: InVisibleRect follows resizing and self handles the two
            // pointer selectors. AppKit does not retain the tracking owner.
            let area=unsafe {NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(),NSRect::ZERO,NSTrackingAreaOptions::MouseEnteredAndExited|NSTrackingAreaOptions::ActiveInKeyWindow|NSTrackingAreaOptions::InVisibleRect,Some(self),None)};
            self.addTrackingArea(&area); *self.ivars().tracking.borrow_mut()=Some(area);
        }
        #[unsafe(method(mouseEntered:))] fn entered(&self,_:&NSEvent) {self.ivars().hovered.set(true);if self.isEnabled(){feedback(self);}}
        #[unsafe(method(mouseExited:))] fn exited(&self,_:&NSEvent) {self.ivars().hovered.set(false);if self.isEnabled(){feedback(self);}}
    }
);
impl Button {
    pub fn new(
        mtm: MainThreadMarker,
        title: &str,
        subtitle: &'static str,
        symbol: Option<&'static str>,
        key: &str,
        style: ButtonStyle,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ButtonIvars {
            style,
            symbol,
            subtitle,
            key: RefCell::new(key.into()),
            hovered: Cell::new(false),
            tracking: RefCell::new(None),
        });
        // SAFETY: NSButton designated initializer on the allocated subclass.
        let this: Retained<Self> = unsafe { msg_send![super(this),initWithFrame:NSRect::ZERO] };
        this.setButtonType(NSButtonType::MomentaryPushIn);
        this.setBordered(false);
        this.setWantsLayer(true);
        this.setFocusRingType(NSFocusRingType::Exterior);
        this.setTitle(&NSString::from_str(title));
        this.setAccessibilityLabel(Some(&NSString::from_str(title)));
        this
    }
    /// # Safety
    /// Target must implement the action with an NSButton argument and outlive this button.
    pub unsafe fn connect(&self, target: &AnyObject, action: Sel) {
        unsafe {
            self.setTarget(Some(target));
            self.setAction(Some(action));
        }
    }
    pub fn key(&self, value: &str) {
        if *self.ivars().key.borrow() != value {
            *self.ivars().key.borrow_mut() = value.into();
            NSView::setNeedsDisplay(self, true);
        }
    }
    pub fn selected(&self, value: bool) {
        let state = if value {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        };
        if self.state() != state {
            self.setState(state);
            feedback(self);
        }
        self.setAccessibilitySelected(value);
    }
    fn draw_content(&self) {
        let b = self.bounds();
        let enabled = self.isEnabled();
        let selected = self.state() == NSControlStateValueOn;
        let pointer_inside = self.window().is_some_and(|window| {
            let p = self.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
            window.isKeyWindow()
                && p.x >= 0.
                && p.y >= 0.
                && p.x < b.size.width
                && p.y < b.size.height
        });
        let hot =
            enabled && ((self.ivars().hovered.get() && pointer_inside) || self.isHighlighted());
        let primary = matches!(self.ivars().style, ButtonStyle::Primary);
        let action = matches!(self.ivars().style, ButtonStyle::Action);
        let radius = if action { 12. } else { 8. };
        if primary {
            fill(b, radius, if enabled { ACCENT } else { RAISED });
        } else if hot || selected {
            fill(b, radius, RAISED);
        } else if action {
            fill(b, radius, SURFACE);
        }
        if action || selected {
            outline(
                rect(0.5, 0.5, b.size.width - 1., b.size.height - 1.),
                radius - 0.5,
                TEXT,
                if hot { 0.18 } else { 0.09 },
            );
        }
        if let ButtonStyle::Swatch(c) = self.ivars().style {
            let swatch = rect(
                (b.size.width - 16.) / 2.,
                (b.size.height - 16.) / 2.,
                16.,
                16.,
            );
            NSColor::colorWithSRGBRed_green_blue_alpha(
                c[0],
                c[1],
                c[2],
                if enabled { 1. } else { 0.4 },
            )
            .setFill();
            NSBezierPath::bezierPathWithOvalInRect(swatch).fill();
            if selected {
                outline(
                    rect(swatch.origin.x - 4., swatch.origin.y - 4., 24., 24.),
                    12.,
                    TEXT,
                    0.9,
                );
            }
            return;
        }
        let ink = if !enabled {
            MUTED
        } else if primary {
            INK
        } else {
            TEXT
        };
        let title = self.title().to_string();
        let key = self.ivars().key.borrow();
        if action {
            let x = 20.;
            if let Some(icon) = self.ivars().symbol {
                symbol(icon, rect(x, 24., 16., 16.), SECONDARY);
            }
            text(&title, x + 28., 20., 16., true, ink);
            text(self.ivars().subtitle, x + 28., 48., 13., false, SECONDARY);
            let w = if key.chars().count() > 1 { 64. } else { 32. };
            keycap(&key, rect(b.size.width - w - 20., 24., w, 32.), false);
        } else {
            let mut x = 12.;
            if let Some(icon) = self.ivars().symbol {
                if title.is_empty() {
                    x = (b.size.width - 16.) / 2.;
                }
                symbol(icon, rect(x, (b.size.height - 16.) / 2., 16., 16.), ink);
                x += 24.;
            }
            if !title.is_empty() {
                if key.is_empty() && self.ivars().symbol.is_none() {
                    centered(&title, b, 13., true, ink, false);
                } else {
                    text(&title, x, (b.size.height - 16.) / 2. - 1., 13., true, ink);
                }
            }
            if !key.is_empty() {
                let w = if key.chars().count() > 1 {
                    key.chars().count() as f64 * 8. + 16.
                } else {
                    24.
                };
                if primary {
                    centered(
                        &key,
                        rect(b.size.width - w - 8., (b.size.height - 24.) / 2., w, 24.),
                        12.,
                        true,
                        INK,
                        false,
                    );
                } else {
                    keycap(
                        &key,
                        rect(b.size.width - w - 8., (b.size.height - 24.) / 2., w, 24.),
                        false,
                    );
                }
            }
        }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    pub struct CopiedView;
    unsafe impl NSObjectProtocol for CopiedView {}
    impl CopiedView {
        #[unsafe(method(isFlipped))] fn is_flipped(&self)->bool {true}
        #[unsafe(method(drawRect:))] fn draw_rect(&self,_:NSRect) {
            panel(self.bounds(),12.);
            symbol("checkmark",rect(16.,16.,16.,16.),ACCENT);
        }
    }
);
impl CopiedView {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSView designated initializer, with no additional ivars.
        let this: Retained<Self> =
            unsafe { msg_send![Self::alloc(mtm),initWithFrame:rect(0.,0.,232.,48.)] };
        this.setWantsLayer(true);
        let label = NSTextField::labelWithString(&NSString::from_str("Copied to clipboard"), mtm);
        label.setFrame(rect(44., 14., 172., 22.));
        label.setFont(Some(&NSFont::systemFontOfSize_weight(14., 0.23)));
        label.setTextColor(Some(&color(TEXT)));
        this.addSubview(&label);
        this
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn easing_is_bounded_monotonic_and_exact_at_endpoints() {
        assert_eq!(ease_out(0.), 0.);
        assert_eq!(ease_out(1.), 1.);
        let mut previous = 0.;
        for n in 0..=1000 {
            let y = ease_out(n as f64 / 1000.);
            assert!(y >= previous && y <= 1.);
            previous = y;
        }
    }
    #[test]
    fn text_tokens_meet_normal_text_contrast() {
        let luminance = |hex| {
            let c = rgba(hex);
            let linear = |v: f64| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            linear(c[0]) * 0.2126 + linear(c[1]) * 0.7152 + linear(c[2]) * 0.0722
        };
        for background in [BACKGROUND, DEEP, SURFACE, RAISED] {
            for foreground in [TEXT, SECONDARY, MUTED, ERROR] {
                let contrast = (luminance(foreground) + 0.05) / (luminance(background) + 0.05);
                assert!(
                    contrast >= 4.5,
                    "{foreground:x} on {background:x}: {contrast}"
                );
            }
        }
        assert!((luminance(ACCENT) + 0.05) / (luminance(INK) + 0.05) >= 4.5);
    }
}
