//! A native, frozen-screen selection and markup surface.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use dispatch2::{DispatchQoS, DispatchQueue, GlobalQueueIdentifier, MainThreadBound};
use objc2::rc::{Retained, Weak, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_core_graphics::CGContext;
use objc2_foundation::{
    NSDictionary, NSPoint, NSRange, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString,
};
use objc2_quartz_core::{CACurrentMediaTime, CADisplayLink, CAFrameRateRange, CATransaction};

use crate::{
    compositor::Compositor,
    editor::{COLORS, CacheRequest, Editor, RasterCacheState, RedactionTone},
    frame_probe::FrameProbe,
    geometry,
    imaging::{self, Image, Mark, Tool},
    model::CaptureMode,
    ui::{self, Button, ButtonStyle},
};

const ACCENT: [f64; 4] = ui::rgba(ui::ACCENT);
const TEXT: [f64; 4] = ui::rgba(ui::TEXT);
const MUTED: [f64; 4] = ui::rgba(ui::MUTED);

pub enum OverlayAction {
    Cancel,
    Copy { image: Image, marks: Vec<Mark> },
    Mode(CaptureMode),
    BeginMarkup,
    SelectionStarted,
    RenderCheckFinished,
    AnnotationCheckFinished(Result<(), String>),
}

struct Zoom {
    start: f64,
    from: NSRect,
    to: NSRect,
}

struct State {
    mode: CaptureMode,
    selection: NSRect,
    start: Option<NSPoint>,
    crop: Option<Image>,
    redaction_selection: NSRect,
    cached_image: Option<Image>,
    cache: RasterCacheState,
    editing: bool,
    busy: bool,
    zoom: Option<Zoom>,
    editor: Editor,
    message: Option<String>,
}

struct ButtonSlot {
    tag: isize,
    button: Retained<Button>,
}

pub struct OverlayIvars {
    image: Image,
    compositor: Compositor,
    foreground: RefCell<Option<Retained<ForegroundView>>>,
    chrome: RefCell<Vec<Retained<ChromeView>>>,
    state: RefCell<State>,
    callback: Rc<dyn Fn(OverlayAction)>,
    buttons: RefCell<Vec<ButtonSlot>>,
    display_link: RefCell<Option<Retained<CADisplayLink>>>,
    frame_pending: Cell<bool>,
    probe: RefCell<Option<FrameProbe>>,
    text_input: RefCell<Option<Retained<AnnotationInput>>>,
    text_origin: Cell<NSPoint>,
    brush_cursor: Retained<NSCursor>,
}

impl Drop for OverlayIvars {
    fn drop(&mut self) {
        if let Some(link) = self.display_link.get_mut().take() {
            link.invalidate();
        }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = OverlayIvars]
    pub struct OverlayView;

    unsafe impl NSObjectProtocol for OverlayView {}

    impl OverlayView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool { false }
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let s=self.ivars().state.borrow();
            let cursor=if s.editing&&s.editor.tool==Tool::Text {NSCursor::IBeamCursor()} else if s.editing&&s.editor.tool==Tool::PixelateBrush{self.ivars().brush_cursor.clone()}else{NSCursor::crosshairCursor()};
            self.addCursorRect_cursor(self.bounds(),&cursor);
        }
        #[unsafe(method(viewDidMoveToWindow))]
        fn moved_to_window(&self) {
            unsafe { let _: () = msg_send![super(self), viewDidMoveToWindow]; }
            if self.window().is_some() { self.request_frame(); }
            else { self.cancel_animation(); }
        }
        #[unsafe(method(layout))]
        fn layout_native(&self) {
            // SAFETY: Calling NSView's inherited implementation with its exact ABI.
            unsafe { let _: () = msg_send![super(self), layout]; }
            self.position_buttons();
            self.request_frame();
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) { self.pointer_down(event); }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) { self.pointer_dragged(event); }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) { self.pointer_up(event); }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) { self.handle_key(event); }
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool { self.key_equivalent(event) }
        #[unsafe(method(toolbarAction:))]
        fn toolbar_action(&self, sender: &NSButton) { self.activate_button(sender.tag()); }
    }
);

define_class!(
    #[unsafe(super(NSTextView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DriverIvars]
    struct AnnotationInput;
    unsafe impl NSObjectProtocol for AnnotationInput {}
    impl AnnotationInput {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            if (self.string().length()==0) && self.ivars().owner.load().is_some_and(|v|v.ivars().state.borrow().editor.tool!=Tool::Text) { std::ptr::null_mut() }
            else { unsafe { msg_send![super(self),hitTest:point] } }
        }
        #[unsafe(method(didChangeText))]
        fn changed(&self) {
            unsafe { let _:()=msg_send![super(self),didChangeText]; }
            if let Some(owner)=self.ivars().owner.load() { owner.refresh_text_input(); }
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if event.isARepeat() && matches!(event.keyCode(),36|76|53) { return; }
            let composing: bool = unsafe { msg_send![self, hasMarkedText] };
            if composing {
                unsafe { let _:()=msg_send![super(self),keyDown:event]; }
                return;
            }
            if let Some(owner)=self.ivars().owner.load() {
                if event.keyCode()==48 {
                    owner.focus_next_button(event.modifierFlags().contains(NSEventModifierFlags::Shift));
                    return;
                }
                if event.keyCode()==53 {
                    if !(self.string().length()==0) { self.setString(&NSString::new()); owner.refresh_text_input(); }
                    else { owner.handle_key(event); } return;
                }
                if matches!(event.keyCode(),36|76) && !event.modifierFlags().contains(NSEventModifierFlags::Shift) {
                    if (self.string().length()==0) || event.modifierFlags().contains(NSEventModifierFlags::Command) { owner.copy_result(); }
                    else { owner.commit_text(); } return;
                }
            }
            unsafe { let _:()=msg_send![super(self),keyDown:event]; }
        }
    }
);

struct DriverIvars {
    owner: Weak<OverlayView>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DriverIvars]
    struct FrameDriver;
    unsafe impl NSObjectProtocol for FrameDriver {}
    impl FrameDriver {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            if let Some(owner) = self.ivars().owner.load() { owner.display_frame(link); }
            else { link.invalidate(); }
        }
    }
);

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DriverIvars]
    struct ForegroundView;
    unsafe impl NSObjectProtocol for ForegroundView {}
    impl ForegroundView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool { false }
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _: NSPoint) -> *mut NSView { std::ptr::null_mut() }
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            if let Some(owner) = self.ivars().owner.load() { owner.draw_surface(); }
        }
    }
);

struct ChromeIvars {
    owner: Weak<OverlayView>,
    toolbar: bool,
}
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ChromeIvars]
    struct ChromeView;
    unsafe impl NSObjectProtocol for ChromeView {}
    impl ChromeView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _: NSPoint) -> *mut NSView { std::ptr::null_mut() }
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _: NSRect) {
            ui::panel(self.bounds(),12.);
            if self.ivars().toolbar {
                for x in [296.,448.,536.] {
                    ui::color(ui::TEXT).colorWithAlphaComponent(0.08).setFill();
                    NSBezierPath::fillRect(rect(x,16.,1.,20.));
                }
            } else if let Some(owner)=self.ivars().owner.load() {
                let state=owner.ivars().state.borrow();
                if state.editing {
                    ui::symbol("pencil.tip",rect(16.,16.,16.,16.),ui::SECONDARY);
                    ui::text(if state.busy {"Copying screenshot…"}else{"Annotate your capture"},44.,16.,13.,true,ui::TEXT);
                } else {
                    ui::logo(rect(16.,12.,24.,24.));
                    ui::color(ui::TEXT).colorWithAlphaComponent(0.08).setFill();
                    NSBezierPath::fillRect(rect(328.,16.,1.,16.));
                }
            }
        }
    }
);

impl OverlayView {
    pub fn check_annotations_start(&self) {
        {
            let mut s = self.ivars().state.borrow_mut();
            s.selection = rect(
                120.,
                120.,
                self.bounds().size.width.min(1000.) - 160.,
                self.bounds().size.height.min(700.) - 160.,
            );
            s.mode = CaptureMode::Markup;
        }
        self.finish_selection();
    }
    pub fn check_annotations_finish(&self) {
        let result = (|| -> Result<(), String> {
            self.focus_text_input();
            let board = NSPasteboard::pasteboardWithUniqueName();
            let sample =
                NSString::from_str("Typed or dictated — café, 日本語\nP A R H 123 stay text");
            if !board.setString_forType(&sample, unsafe { NSPasteboardTypeString }) {
                return Err("Could not prepare private pasteboard".into());
            }
            let input = self
                .ivars()
                .text_input
                .borrow()
                .as_ref()
                .cloned()
                .ok_or("Missing native text input")?;
            if !input.readSelectionFromPasteboard(&board) || self.draft() != sample.to_string() {
                return Err("Native paste/Unicode insertion failed".into());
            }
            let ax: Option<Retained<AnyObject>> = unsafe { msg_send![&*input, accessibilityValue] };
            let ax = ax.ok_or("Native text has no Accessibility value")?;
            let ax = ax
                .downcast_ref::<NSString>()
                .ok_or("Native Accessibility value is not text")?;
            if ax.to_string() != sample.to_string() {
                return Err("Dictation readback differs from editor text".into());
            }
            let range: NSRange = unsafe { msg_send![&*input, accessibilitySelectedTextRange] };
            if range != input.selectedRange() {
                return Err("Dictation selection readback differs from editor selection".into());
            }
            let key = |code, chars, repeat| {
                NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    NSEventType::KeyDown, NSPoint::ZERO, NSEventModifierFlags::empty(), 0.,
                    self.window().map(|w|w.windowNumber()).unwrap_or(0), None,
                    &NSString::from_str(chars), &NSString::from_str(chars), repeat, code,
                ).ok_or("Could not construct native key diagnostic")
            };
            input.key_down(sel!(keyDown:), &*key(36, "\r", false)?);
            if !self.draft().is_empty() || self.ivars().state.borrow().editor.marks.len() != 1 {
                return Err("Return did not commit exactly one text annotation".into());
            }
            input.key_down(sel!(keyDown:), &*key(36, "\r", true)?);
            if self.ivars().state.borrow().busy {
                return Err("Held Return copied the capture after committing text".into());
            }
            input.key_down(sel!(keyDown:), &*key(48, "\t", false)?);
            if self.text_has_focus() || !self.draft().is_empty() {
                return Err("Tab did not move from native text to the toolbar".into());
            }
            self.focus_text_input();
            {
                let mut s = self.ivars().state.borrow_mut();
                if s.editor.marks.last().is_none_or(|m| m.tool != Tool::Text)
                    || s.editor.tool != Tool::Pen
                {
                    return Err("Typing changed the drawing tool".into());
                }
                let stamp = s.editor.marks.last().unwrap().stamp.as_ref().unwrap();
                if !stamp
                    .pixels
                    .pixels()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[3] > 0)
                {
                    return Err("Text raster is empty".into());
                }
                let Some(crop) = s.crop.as_ref() else {
                    return Err("No crop".into());
                };
                let w = crop.width() as f64;
                let h = crop.height() as f64;
                if let Some((revision, seed, tone)) = s.editor.pixelation_job() {
                    let palette = tone.palette(imaging::redaction_palette(
                        &self.ivars().image,
                        s.redaction_selection,
                        self.bounds(),
                    ));
                    let stamp = crate::drawing::Stamp::new(crate::redaction::generate(
                        w as usize, h as usize, 16, seed, palette,
                    )?)?;
                    s.editor.complete_pixelation(revision, Some(stamp));
                }
                if !s.editor.pixelation_ready() {
                    return Err("Redaction preparation timed out".into());
                }
                for (tool, a, b) in [
                    (
                        Tool::PixelateArea,
                        NSPoint::new(w * 0.08, h * 0.34),
                        NSPoint::new(w * 0.45, h * 0.64),
                    ),
                    (
                        Tool::PixelateBrush,
                        NSPoint::new(w * 0.6, h * 0.4),
                        NSPoint::new(w * 0.82, h * 0.7),
                    ),
                ] {
                    s.editor.tool = tool;
                    s.editor.begin(a, 2.);
                    s.editor.drag(b);
                    s.editor.end(b);
                }
                let count = s.editor.marks.len();
                s.editor.undo();
                s.editor.redo();
                if s.editor.marks.len() != count {
                    return Err("History lost an annotation".into());
                }
                s.editor.tool = Tool::PixelateArea;
                let rendered = imaging::render(s.crop.as_ref().unwrap(), &s.editor.marks)?;
                std::fs::create_dir_all("artifacts").map_err(|e| e.to_string())?;
                std::fs::write(
                    "artifacts/tray-annotation-fixture.png",
                    imaging::encode_png(&rendered)?,
                )
                .map_err(|e| e.to_string())?;
            }
            self.refresh_buttons();
            for slot in self.ivars().buttons.borrow().iter() {
                if ((30..=33).contains(&slot.tag) && !slot.button.isHidden())
                    || ((34..=36).contains(&slot.tag) && slot.button.isHidden())
                {
                    return Err("Redaction tone controls did not replace pen colors".into());
                }
            }
            self.refresh_text_input();
            self.setNeedsDisplay(true);
            self.displayIfNeeded();
            if let Some(bitmap) = self.bitmapImageRepForCachingDisplayInRect(self.bounds()) {
                self.cacheDisplayInRect_toBitmapImageRep(self.bounds(), &bitmap);
                if let Some(cg) = bitmap.CGImage() {
                    let image = imaging::Image::from_cg(cg.into())?;
                    std::fs::write(
                        "artifacts/tray-native-interface.png",
                        imaging::encode_png(&image)?,
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        })();
        (self.ivars().callback)(OverlayAction::AnnotationCheckFinished(result));
    }
    fn key_equivalent(&self, event: &NSEvent) -> bool {
        if event.keyCode() == 48 {
            self.focus_next_button(event.modifierFlags().contains(NSEventModifierFlags::Shift));
            return true;
        }
        if self.is_editing() && self.text_has_focus() {
            if matches!(event.keyCode(), 126 | 125) {
                self.handle_key(event);
                return true;
            }
            let ch = event
                .charactersIgnoringModifiers()
                .map(|s| s.to_string().to_lowercase())
                .unwrap_or_default();
            if self.draft().is_empty()
                && event
                    .modifierFlags()
                    .contains(NSEventModifierFlags::Command)
                && matches!(ch.as_str(), "z" | "c")
            {
                self.handle_key(event);
                return true;
            }
            return unsafe { msg_send![super(self), performKeyEquivalent:event] };
        }
        {
            let ch = event
                .charactersIgnoringModifiers()
                .map(|s| s.to_string().to_lowercase())
                .unwrap_or_default();
            let command = event
                .modifierFlags()
                .contains(NSEventModifierFlags::Command);
            // These commands keep working when Tab places focus on a toolbar
            // button. Space remains available to activate the focused button.
            if matches!(event.keyCode(), 53 | 126 | 125 | 36 | 76)
                || (command && matches!(ch.as_str(), "z" | "c"))
            {
                self.handle_key(event);
                true
            } else {
                // SAFETY: Exact NSResponder method signature and valid event.
                unsafe { msg_send![super(self), performKeyEquivalent: event] }
            }
        }
    }
    pub fn new(
        mtm: MainThreadMarker,
        image: Image,
        frame: NSRect,
        mode: CaptureMode,
        callback: Rc<dyn Fn(OverlayAction)>,
    ) -> Retained<Self> {
        let compositor = Compositor::new(mtm, &image, frame);
        let this = Self::alloc(mtm).set_ivars(OverlayIvars {
            image,
            compositor,
            foreground: RefCell::new(None),
            chrome: RefCell::new(Vec::new()),
            callback,
            state: RefCell::new(State {
                mode,
                selection: NSRect::ZERO,
                start: None,
                crop: None,
                redaction_selection: NSRect::ZERO,
                cached_image: None,
                cache: RasterCacheState::default(),
                editing: false,
                busy: false,
                zoom: None,
                editor: Editor::default(),
                message: None,
            }),
            buttons: RefCell::new(Vec::new()),
            display_link: RefCell::new(None),
            frame_pending: Cell::new(true),
            probe: RefCell::new(None),
            text_input: RefCell::new(None),
            text_origin: Cell::new(NSPoint::new(16., 16.)),
            brush_cursor: ui::pixel_brush_cursor(mtm),
        });
        // SAFETY: Allocated NSView subclass is initialized once with a valid frame.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        this.setAppearance(
            NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref(),
        );
        this.addSubview(this.ivars().compositor.view());
        let foreground = ForegroundView::alloc(mtm).set_ivars(DriverIvars {
            owner: Weak::from_retained(&this),
        });
        let foreground: Retained<ForegroundView> =
            unsafe { msg_send![super(foreground), initWithFrame: frame] };
        foreground.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        foreground.setWantsLayer(true);
        foreground.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        this.addSubview(&foreground);
        *this.ivars().foreground.borrow_mut() = Some(foreground);
        // Static panels have small, independent backing layers. Pointer frames
        // redraw only the selection/annotations, never toolbar text or symbols.
        for toolbar in [false, true] {
            let chrome = ChromeView::alloc(mtm).set_ivars(ChromeIvars {
                owner: Weak::from_retained(&this),
                toolbar,
            });
            // SAFETY: NSView initializer on the allocated native subclass.
            let chrome: Retained<ChromeView> =
                unsafe { msg_send![super(chrome),initWithFrame:NSRect::ZERO] };
            chrome.setWantsLayer(true);
            chrome.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
            this.addSubview(&chrome);
            this.ivars().chrome.borrow_mut().push(chrome);
        }
        this.build_buttons();
        let input = AnnotationInput::alloc(mtm).set_ivars(DriverIvars {
            owner: Weak::from_retained(&this),
        });
        let input: Retained<AnnotationInput> =
            unsafe { msg_send![super(input),initWithFrame:NSRect::ZERO] };
        input.setRichText(false);
        input.setVerticallyResizable(false);
        input.setHorizontallyResizable(false);
        input.setWantsLayer(true);
        if let Some(layer) = input.layer() {
            layer.setCornerRadius(8.);
            layer.setMasksToBounds(true);
        }
        input.setAllowsUndo(true);
        input.setImportsGraphics(false);
        input.setDrawsBackground(false);
        input.setFont(Some(&NSFont::systemFontOfSize_weight(24., 0.3)));
        input.setTextContainerInset(NSSize::new(4., 4.));
        if let Some(container) = unsafe { input.textContainer() } {
            container.setLineFragmentPadding(0.);
        }
        input.setAccessibilityLabel(Some(&NSString::from_str("Annotation text")));
        input.setAccessibilityHelp(Some(&NSString::from_str("Type, paste, or dictate. Return places text. Shift–Return adds a line. Down copies the capture.")));
        input.setHidden(true);
        this.addSubview(&input);
        *this.ivars().text_input.borrow_mut() = Some(input);
        this.refresh_buttons();
        this
    }

    pub fn set_mode(&self, mode: CaptureMode) {
        {
            let mut s = self.ivars().state.borrow_mut();
            if s.editing || s.busy {
                return;
            }
            s.mode = mode;
        }
        self.refresh_buttons();
        self.request_frame();
    }

    pub fn handle_arrow(&self, mode: CaptureMode) {
        let (busy, editing) = {
            let s = self.ivars().state.borrow();
            (s.busy, s.editing)
        };
        if busy {
            return;
        }
        if editing {
            self.copy_result();
        } else {
            (self.ivars().callback)(OverlayAction::Mode(mode));
        }
    }

    pub fn is_editing(&self) -> bool {
        self.ivars().state.borrow().editing
    }

    pub fn clear_selection(&self) {
        {
            let mut s = self.ivars().state.borrow_mut();
            if s.editing {
                return;
            }
            s.selection = NSRect::ZERO;
            s.start = None;
        }
        self.request_frame();
    }

    pub fn show_error(&self, message: &str) {
        let should_edit = {
            let mut s = self.ivars().state.borrow_mut();
            s.message = Some(message.to_owned());
            s.busy = false;
            s.crop.is_some() && !s.editing
        };
        if should_edit {
            self.begin_markup();
        }
        self.focus_text_input();
        self.schedule_cache();
        self.refresh_buttons();
        self.request_frame();
    }

    pub fn set_busy(&self, busy: bool) {
        self.ivars().state.borrow_mut().busy = busy;
        self.refresh_text_input();
        self.refresh_buttons();
        self.request_frame();
    }

    pub fn cancel_animation(&self) {
        if let Some(link) = self.ivars().display_link.borrow_mut().take() {
            link.invalidate();
        }
        self.ivars().state.borrow_mut().zoom = None;
    }

    pub fn frame_clock_is_paused(&self) -> bool {
        self.ivars()
            .display_link
            .borrow()
            .as_ref()
            .is_none_or(|link| link.isPaused())
    }

    fn request_frame(&self) {
        self.ivars().frame_pending.set(true);
        if self.window().is_none() {
            return;
        }
        let mut slot = self.ivars().display_link.borrow_mut();
        if let Some(link) = slot.as_ref() {
            link.setPaused(false);
            return;
        }
        // The run loop retains the driver, which holds only a weak view. A
        // paused link is invalidated explicitly when the overlay is dismissed.
        let owner =
            unsafe { Retained::retain(self as *const Self as *mut Self) }.expect("live view");
        let driver = FrameDriver::alloc(self.mtm()).set_ivars(DriverIvars {
            owner: Weak::from_retained(&owner),
        });
        let driver: Retained<FrameDriver> = unsafe { msg_send![super(driver), init] };
        // SAFETY: Correct target/selector ABI, available since our macOS 14
        // minimum. This link follows the view's actual display, not the main
        // screen or a fixed timer interval.
        let link = unsafe { self.displayLinkWithTarget_selector(&driver, sel!(tick:)) };
        if let Some(screen) = self.window().and_then(|window| window.screen()) {
            let hz = crate::refresh::target_hz(screen.maximumFramesPerSecond() as f64) as f32;
            if hz > 0.0 {
                link.setPreferredFrameRateRange(CAFrameRateRange::new(hz, hz, hz));
            }
        }
        // Common modes keep frames flowing during pointer tracking.
        unsafe {
            link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }
        *slot = Some(link);
    }

    fn display_frame(&self, link: &CADisplayLink) {
        let measured = self
            .ivars()
            .probe
            .borrow()
            .as_ref()
            .map(|_| std::time::Instant::now());
        if measured.is_some() {
            self.advance_render_check();
        }
        let zooming = self.ivars().state.borrow().zoom.is_some();
        if !self.ivars().frame_pending.replace(false) && !zooming {
            link.setPaused(true);
            return;
        }
        {
            let mut s = self.ivars().state.borrow_mut();
            if let Some(z) = s.zoom.as_ref() {
                // Draw the state for the upcoming presentation, not the
                // previous refresh. A delayed frame jumps to the right time.
                let t = ((link.targetTimestamp() - z.start) / ui::ZOOM).clamp(0.0, 1.0);
                s.selection = lerp_rect(z.from, z.to, ui::ease_out(t));
                if t >= 1.0 {
                    s.zoom = None;
                }
            }
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        {
            let s = self.ivars().state.borrow();
            self.ivars().compositor.update(
                self.bounds(),
                s.selection,
                s.zoom.as_ref().map_or(s.selection, |z| z.to),
                s.editing,
                s.cached_image.as_ref(),
            );
        }
        if let Some(foreground) = self.ivars().foreground.borrow().as_ref() {
            foreground.setNeedsDisplay(true);
            // Finish this frame inside the display-link callback instead of
            // waiting for AppKit's next ordinary window-update pass.
            foreground.displayIfNeeded();
        }
        CATransaction::commit();
        if zooming && self.ivars().state.borrow().zoom.is_none() {
            self.focus_text_input();
        }
        if let (Some(started), Some(probe)) = (measured, self.ivars().probe.borrow_mut().as_mut()) {
            probe.record(link.timestamp(), started);
        }
    }

    /// Runs only when the app explicitly starts its synthetic, private-clipboard
    /// diagnostic. The same compositor, zoom, and stroke renderer are exercised.
    pub fn check_rendering(&self) {
        if let Some(screen) = self.window().and_then(|w| w.screen()) {
            println!(
                "RENDER CHECK: screen_max_hz={}, scale={}",
                screen.maximumFramesPerSecond(),
                screen.backingScaleFactor()
            );
        }
        *self.ivars().probe.borrow_mut() = Some(FrameProbe::new());
        self.request_frame();
    }

    fn advance_render_check(&self) {
        let (elapsed, stage) = {
            let probe = self.ivars().probe.borrow();
            let Some(probe) = probe.as_ref() else {
                return;
            };
            (probe.started.elapsed().as_secs_f64(), probe.stage)
        };
        if elapsed >= 5.25 {
            if let Some(probe) = self.ivars().probe.borrow_mut().take() {
                probe.report();
            }
            (self.ivars().callback)(OverlayAction::RenderCheckFinished);
            return;
        }
        self.ivars().frame_pending.set(true);
        let b = self.bounds();
        if elapsed < 2.0 {
            let mut s = self.ivars().state.borrow_mut();
            let start = NSPoint::new(b.size.width * 0.2, b.size.height * 0.25);
            let end = NSPoint::new(
                b.size.width * (0.65 + 0.15 * (elapsed * 9.).sin()),
                b.size.height * (0.55 + 0.15 * (elapsed * 7.).cos()),
            );
            s.start = Some(start);
            s.selection = geometry::selection(start, end, b);
        } else if stage == 0 {
            self.ivars().probe.borrow_mut().as_mut().unwrap().stage = 1;
            {
                let mut s = self.ivars().state.borrow_mut();
                s.start = None;
                s.mode = CaptureMode::Markup;
            }
            self.finish_selection();
        } else if elapsed >= 2.25 {
            self.ivars().probe.borrow_mut().as_mut().unwrap().stage = 2;
            let mut s = self.ivars().state.borrow_mut();
            if s.zoom.is_some() {
                return;
            }
            let Some(crop) = s.crop.as_ref() else {
                return;
            };
            let size = image_size(crop);
            let t = (elapsed - 2.25) / 3.0;
            let pixel = NSPoint::new(
                size.width * (0.1 + 0.8 * t),
                size.height * (0.5 + 0.3 * (t * 24.).sin()),
            );
            if s.editor.current.is_none() {
                let scale = size.width / s.selection.size.width;
                s.editor.begin(pixel, scale);
            } else {
                s.editor.drag(pixel);
            }
        }
    }

    pub fn restore_focus(&self) {
        if self.is_editing() {
            self.focus_text_input();
        } else {
            self.focus();
        }
    }
    fn focus(&self) {
        if let Some(window) = self.window() {
            window.makeKeyWindow();
            window.makeFirstResponder(Some(self));
        }
    }
    fn draft(&self) -> String {
        self.ivars()
            .text_input
            .borrow()
            .as_ref()
            .map(|t| t.string().to_string())
            .unwrap_or_default()
    }
    fn text_has_focus(&self) -> bool {
        let input = self.ivars().text_input.borrow();
        input.as_ref().is_some_and(|input| {
            self.window()
                .and_then(|w| w.firstResponder())
                .is_some_and(|r| {
                    Retained::as_ptr(&r).cast::<()>() == Retained::as_ptr(input).cast::<()>()
                })
        })
    }
    fn refresh_text_input(&self) {
        let s = self.ivars().state.borrow();
        let input = self.ivars().text_input.borrow();
        let Some(input) = input.as_ref() else {
            return;
        };
        if !s.editing || s.busy || s.zoom.is_some() {
            input.setHidden(true);
            return;
        }
        let Some(crop) = s.crop.as_ref() else {
            return;
        };
        let ratio = s.selection.size.width / crop.width() as f64;
        let mut origin = self.ivars().text_origin.get();
        origin.x = origin
            .x
            .max(0.)
            .min((crop.width() as f64 - 96. / ratio).max(0.));
        origin.y = origin
            .y
            .max(0.)
            .min((crop.height() as f64 - 64. / ratio).max(0.));
        self.ivars().text_origin.set(origin);
        let x = s.selection.origin.x + origin.x * ratio;
        let y = s.selection.origin.y + origin.y * ratio;
        let max_width = (s.selection.origin.x + s.selection.size.width - x).clamp(1., 560.);
        let max_height = (s.selection.origin.y + s.selection.size.height - y).clamp(1., 240.);
        let text = input.string();
        let measured = unsafe {
            text.boundingRectWithSize_options_attributes_context(
                NSSize::new((max_width - 8.).max(1.), 4096.),
                NSStringDrawingOptions::UsesLineFragmentOrigin,
                Some(&ui::attributes(24., true, ui::TEXT, false)),
                None,
            )
        };
        let width = (((measured.size.width + 8.) / 4.).ceil() * 4.)
            .max(96.)
            .min(max_width);
        let height = (((measured.size.height + 8.) / 4.).ceil() * 4.)
            .max(40.)
            .min(max_height);
        input.setFrame(rect(x, y, width, height));
        let visible = !(input.string().length() == 0) || s.editor.tool == Tool::Text;
        input.setDrawsBackground(visible);
        input.setBackgroundColor(&ui::color(ui::DEEP));
        input.setTextColor(Some(&color(COLORS[s.editor.color_index])));
        let caret = if visible {
            ui::color(ui::ACCENT)
        } else {
            NSColor::clearColor()
        };
        input.setInsertionPointColor(Some(&caret));
        input.setHidden(false);
        input.setNeedsDisplay(true);
    }
    fn focus_text_input(&self) {
        self.refresh_text_input();
        if self.ivars().state.borrow().zoom.is_some() {
            return;
        }
        if let Some(input) = self.ivars().text_input.borrow().as_ref()
            && let Some(window) = self.window()
        {
            window.makeFirstResponder(Some(input));
        }
    }
    fn commit_text(&self) -> bool {
        let text = self.draft();
        if text.is_empty() {
            return true;
        }
        if text.trim().is_empty() {
            if let Some(input) = self.ivars().text_input.borrow().as_ref() {
                input.setString(&NSString::new());
            }
            self.refresh_text_input();
            return true;
        }
        let info = {
            let s = self.ivars().state.borrow();
            if !s.editing || s.busy {
                return false;
            }
            let Some(crop) = s.crop.as_ref() else {
                return false;
            };
            let scale = crop.width() as f64 / s.selection.size.width;
            let p = self.ivars().text_origin.get();
            (
                p,
                scale,
                (crop.width() as f64 - p.x).min(560. * scale),
                COLORS[s.editor.color_index],
                crop.height() as f64 - p.y,
            )
        };
        let result = imaging::text_stamp(self.mtm(), &text, info.3, info.1, info.2, info.4);
        let accepted = match result {
            Ok(stamp) => self
                .ivars()
                .state
                .borrow_mut()
                .editor
                .insert_text(info.0, stamp),
            Err(error) => {
                self.ivars().state.borrow_mut().message = Some(error);
                false
            }
        };
        if accepted {
            if let Some(input) = self.ivars().text_input.borrow().as_ref() {
                input.setString(&NSString::new());
            }
            if self.ivars().state.borrow().editor.tool.is_pixelation() {
                self.warm_pixelation();
            }
            self.schedule_cache();
            self.refresh_text_input();
            self.refresh_buttons();
            self.request_frame();
        } else {
            self.request_frame();
        }
        accepted
    }
    fn warm_pixelation(&self) {
        let (width, height, cell, revision, seed, tone, selection) = {
            let mut s = self.ivars().state.borrow_mut();
            let Some(crop) = s.crop.clone() else {
                return;
            };
            let canvas = s.zoom.as_ref().map_or(s.selection, |z| z.to);
            let cell = ((12. * crop.width() as f64 / canvas.size.width).round() as usize).max(1);
            let Some((revision, seed, tone)) = s.editor.pixelation_job() else {
                return;
            };
            (
                crop.width(),
                crop.height(),
                cell,
                revision,
                seed,
                tone,
                s.redaction_selection,
            )
        };
        let source = self.ivars().image.clone();
        let bounds = self.bounds();
        let owner = unsafe { Retained::retain(self as *const Self as *mut Self) }.unwrap();
        let owner = MainThreadBound::new(Weak::from_retained(&owner), self.mtm());
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInitiated,
        ))
        .exec_async(move || {
            let result = autoreleasepool(|_| {
                let automatic = if tone == RedactionTone::Auto {
                    imaging::redaction_palette(&source, selection, bounds)
                } else {
                    crate::redaction::Palette::DARK
                };
                crate::redaction::generate(width, height, cell, seed, tone.palette(automatic))
                    .and_then(crate::drawing::Stamp::new)
            });
            DispatchQueue::main().exec_async(move || {
                let mtm = MainThreadMarker::new().unwrap();
                if let Some(view) = owner.get(mtm).load() {
                    let again = {
                        let mut s = view.ivars().state.borrow_mut();
                        let error = result.as_ref().err().cloned();
                        let stale = s.editor.complete_pixelation(revision, result.ok());
                        if !stale {
                            if let Some(error) = error {
                                s.message = Some(error);
                            } else if s.message.as_deref() == Some("Preparing redaction…") {
                                s.message = None;
                            }
                        }
                        stale && s.editor.tool.is_pixelation()
                    };
                    if again {
                        view.warm_pixelation();
                    }
                    view.refresh_buttons();
                    view.request_frame();
                }
            });
        });
    }

    fn focus_next_button(&self, reverse: bool) {
        let Some(window) = self.window() else {
            return;
        };
        let slots = self.ivars().buttons.borrow();
        let buttons: Vec<_> = slots
            .iter()
            .filter(|s| !s.button.isHidden() && s.button.isEnabled())
            .collect();
        if buttons.is_empty() {
            return;
        }
        let current = window.firstResponder().and_then(|current| {
            buttons.iter().position(|s| {
                Retained::as_ptr(&current).cast::<()>() == Retained::as_ptr(&s.button).cast::<()>()
            })
        });
        let next = match (current, reverse) {
            (Some(index), false) => (index + 1) % buttons.len(),
            (Some(index), true) => (index + buttons.len() - 1) % buttons.len(),
            (None, false) => 0,
            (None, true) => buttons.len() - 1,
        };
        window.makeFirstResponder(Some(&buttons[next].button));
    }

    fn pointer_down(&self, event: &NSEvent) {
        if !self.commit_text() {
            return;
        }
        self.focus();
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let selecting = {
            let mut s = self.ivars().state.borrow_mut();
            if s.busy || s.zoom.is_some() {
                return;
            }
            s.message = None;
            if s.editing {
                if !contains(s.selection, point) {
                    return;
                }
                if let Some(crop) = s.crop.as_ref() {
                    let size = image_size(crop);
                    let pixel = geometry::image_point(point, s.selection, size);
                    let scale = size.width / s.selection.size.width;
                    self.ivars().text_origin.set(pixel);
                    if s.editor.tool == Tool::Text {
                        drop(s);
                        self.focus_text_input();
                        self.refresh_text_input();
                        return;
                    }
                    if s.editor.tool.is_pixelation() && !s.editor.pixelation_ready() {
                        s.message = Some("Preparing redaction…".into());
                        drop(s);
                        self.warm_pixelation();
                        self.request_frame();
                        return;
                    }
                    if !s.editor.begin(pixel, scale) {
                        s.message = Some(
                            "This canvas is full. Copy it, or undo a mark to keep drawing.".into(),
                        );
                    }
                }
                false
            } else {
                s.start = Some(point);
                s.selection = NSRect::ZERO;
                s.crop = None;
                s.cached_image = None;
                s.cache.reset();
                true
            }
        };
        if selecting {
            (self.ivars().callback)(OverlayAction::SelectionStarted);
        }
        self.request_frame();
    }

    fn pointer_dragged(&self, event: &NSEvent) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        {
            let mut s = self.ivars().state.borrow_mut();
            if s.busy || s.zoom.is_some() {
                return;
            }
            if s.editing {
                if let Some(crop) = s.crop.as_ref() {
                    let pixel = geometry::image_point(point, s.selection, image_size(crop));
                    if !s.editor.drag(pixel) {
                        s.message =
                            Some("This stroke is very long. Release to start another.".into());
                    }
                }
            } else if let Some(start) = s.start {
                s.selection = geometry::selection(start, point, self.bounds());
            }
        }
        self.request_frame();
    }

    fn pointer_up(&self, event: &NSEvent) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let finish = {
            let mut s = self.ivars().state.borrow_mut();
            if s.busy || s.zoom.is_some() {
                return;
            }
            if s.editing {
                if let Some(crop) = s.crop.as_ref() {
                    let pixel = geometry::image_point(point, s.selection, image_size(crop));
                    s.editor.end(pixel);
                }
                false
            } else if let Some(start) = s.start.take() {
                s.selection = geometry::selection(start, point, self.bounds());
                true
            } else {
                false
            }
        };
        if finish {
            self.finish_selection();
        } else {
            self.schedule_cache();
        }
        self.refresh_buttons();
        self.request_frame();
        if self.is_editing() {
            self.focus_text_input();
        }
    }

    fn finish_selection(&self) {
        let selection = self.ivars().state.borrow().selection;
        let crop = match imaging::crop(&self.ivars().image, selection, self.bounds()) {
            Ok(image) => image,
            Err(_) => {
                self.ivars().state.borrow_mut().selection = NSRect::ZERO;
                self.request_frame();
                return;
            }
        };
        let mode = {
            let mut s = self.ivars().state.borrow_mut();
            s.crop = Some(crop.clone());
            s.redaction_selection = selection;
            s.cached_image = Some(crop);
            s.cache.reset();
            s.mode
        };
        if mode == CaptureMode::Copy {
            self.copy_result();
        } else {
            self.begin_markup();
        }
    }

    fn begin_markup(&self) {
        // Keep the display link through the selection-to-canvas transition;
        // recreating it here would discard its established refresh cadence.
        self.ivars().state.borrow_mut().zoom = None;
        let reduce_motion = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        {
            let mut s = self.ivars().state.borrow_mut();
            let Some(crop) = s.crop.as_ref() else {
                return;
            };
            let b = self.bounds();
            let margin_x = 48.0_f64.min(b.size.width * 0.08);
            let margin_y = 48.0_f64.min(b.size.height * 0.2);
            let bottom = 128.0_f64.min(b.size.height * 0.25);
            let area = rect(
                margin_x,
                margin_y,
                (b.size.width - 2.0 * margin_x).max(1.0),
                (b.size.height - margin_y - bottom).max(1.0),
            );
            let target = geometry::fitted_canvas(image_size(crop), area);
            s.editing = true;
            if reduce_motion {
                s.selection = target;
            } else {
                s.zoom = Some(Zoom {
                    start: CACurrentMediaTime(),
                    from: s.selection,
                    to: target,
                });
            }
        }
        (self.ivars().callback)(OverlayAction::BeginMarkup);
        self.warm_pixelation();
        self.refresh_buttons();
        self.focus();
        self.request_frame();
        if reduce_motion {
            self.focus_text_input();
        }
    }

    fn copy_result(&self) {
        if !self.commit_text() {
            return;
        }
        let action = {
            let mut s = self.ivars().state.borrow_mut();
            if s.busy {
                return;
            }
            let Some(image) = s.crop.clone() else {
                return;
            };
            s.editor.commit_current();
            s.busy = true;
            Some(OverlayAction::Copy {
                image,
                marks: s.editor.marks.clone(),
            })
        };
        self.refresh_text_input();
        self.refresh_buttons();
        self.request_frame();
        if let Some(action) = action {
            (self.ivars().callback)(action);
        }
    }

    /// Only history mutations schedule work. Pointer movement uses the previous
    /// immutable raster and draws the small uncached tail plus the active stroke.
    /// At most one background job exists, so rapid undo cannot fan out bitmap
    /// allocations or retain a chain of old frames.
    fn schedule_cache(&self) {
        let job = {
            let mut s = self.ivars().state.borrow_mut();
            if s.busy {
                return;
            }
            let Some(image) = s.cached_image.clone() else {
                return;
            };
            let count = s.editor.marks.len();
            let Some(request) = s.cache.request(count) else {
                return;
            };
            let marks = s.editor.marks[request.from..request.to].to_vec();
            (request, image, marks)
        };
        // SAFETY: A method receiver is live here; the temporary retain only
        // creates a weak reference and is dropped before work is dispatched.
        let owner =
            unsafe { Retained::retain(self as *const Self as *mut Self) }.expect("live view");
        let owner = MainThreadBound::new(Weak::from_retained(&owner), self.mtm());
        let (request, image, marks) = job;
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInitiated,
        ))
        .exec_async(move || {
            let result = autoreleasepool(|_| imaging::render(&image, &marks));
            DispatchQueue::main().exec_async(move || {
                autoreleasepool(|_| {
                    let mtm = MainThreadMarker::new().expect("main dispatch queue");
                    if let Some(view) = owner.get(mtm).load() {
                        view.cache_completed(request, result);
                    }
                });
            });
        });
    }

    fn cache_completed(&self, request: CacheRequest, result: Result<Image, String>) {
        {
            let mut s = self.ivars().state.borrow_mut();
            if let (true, Ok(image)) = (s.cache.complete(request, result.is_ok()), result) {
                s.cached_image = Some(image);
            }
        }
        self.request_frame();
        self.schedule_cache();
    }

    fn change_history(&self, redo: bool) {
        let reset = {
            let mut s = self.ivars().state.borrow_mut();
            let before = s.editor.marks.len();
            if redo {
                s.editor.redo();
            } else {
                s.editor.undo();
            }
            let after = s.editor.marks.len();
            if after < before && s.cache.truncate(after) {
                s.crop.clone()
            } else {
                None
            }
        };
        if let Some(image) = reset {
            let mut s = self.ivars().state.borrow_mut();
            s.cached_image = Some(image);
        }
        if self.ivars().state.borrow().editor.tool.is_pixelation() {
            self.warm_pixelation();
        }
        self.schedule_cache();
        self.refresh_buttons();
        self.request_frame();
    }

    fn handle_key(&self, event: &NSEvent) {
        let key = event.keyCode();
        if key == 53 {
            self.cancel_animation();
            (self.ivars().callback)(OverlayAction::Cancel);
            return;
        }
        if self.ivars().state.borrow().busy
            || (event.isARepeat() && matches!(key, 126 | 125 | 36 | 76 | 49))
        {
            return;
        }
        match key {
            48 => {
                self.focus_next_button(event.modifierFlags().contains(NSEventModifierFlags::Shift));
            }
            126 => self.handle_arrow(CaptureMode::Copy),
            125 => self.handle_arrow(CaptureMode::Markup),
            36 | 76 => self.copy_result(),
            49 => {
                if !self.is_editing() {
                    {
                        let mut s = self.ivars().state.borrow_mut();
                        s.selection = self.bounds();
                        s.start = None;
                    }
                    self.finish_selection();
                }
            }
            _ => {
                let ch = event
                    .charactersIgnoringModifiers()
                    .map(|s| s.to_string().to_lowercase())
                    .unwrap_or_default();
                if event
                    .modifierFlags()
                    .contains(NSEventModifierFlags::Command)
                {
                    match ch.as_str() {
                        "c" => self.copy_result(),
                        "z" => {
                            self.change_history(
                                event.modifierFlags().contains(NSEventModifierFlags::Shift),
                            );
                        }
                        _ => {}
                    }
                } else if self.is_editing() {
                    self.focus_text_input();
                    if let Some(input) = self.ivars().text_input.borrow().as_ref() {
                        unsafe {
                            let _: () = msg_send![input,keyDown:event];
                        }
                    }
                }
            }
        }
    }

    fn activate_button(&self, tag: isize) {
        if (20..=26).contains(&tag) && !self.commit_text() {
            return;
        }
        if tag == 12 {
            self.cancel_animation();
            (self.ivars().callback)(OverlayAction::Cancel);
            return;
        }
        if self.ivars().state.borrow().busy {
            return;
        }
        match tag {
            10 => self.handle_arrow(CaptureMode::Copy),
            11 => self.handle_arrow(CaptureMode::Markup),
            20..=26 => self.ivars().state.borrow_mut().editor.tool = Tool::ALL[(tag - 20) as usize],
            30..=33 => self.ivars().state.borrow_mut().editor.color_index = (tag - 30) as usize,
            34..=36 => self
                .ivars()
                .state
                .borrow_mut()
                .editor
                .set_redaction_tone(RedactionTone::ALL[(tag - 34) as usize]),
            40 => self.change_history(false),
            41 => self.copy_result(),
            42 => self.change_history(true),
            _ => {}
        }
        if self.ivars().state.borrow().editor.tool.is_pixelation() {
            self.warm_pixelation();
        }
        self.refresh_buttons();
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_frame();
        if self.is_editing() {
            self.focus_text_input();
        } else {
            self.focus();
        }
    }

    fn build_buttons(&self) {
        let definitions = [
            (
                10,
                "Copy",
                Some("square.on.square"),
                "↑",
                "Capture and copy (↑)",
            ),
            (11, "Draw", Some("pencil.tip"), "↓", "Capture and draw (↓)"),
            (12, "", Some("xmark"), "", "Cancel capture (Esc)"),
            (20, "", Some("pencil.tip"), "", "Pen"),
            (21, "", Some("arrow.up.right"), "", "Arrow"),
            (22, "", Some("square"), "", "Rectangle"),
            (23, "", Some("highlighter"), "", "Highlighter"),
            (
                24,
                "",
                Some("textformat"),
                "",
                "Text · Or just start typing",
            ),
            (
                25,
                "",
                Some("square.dashed.inset.filled"),
                "",
                "Redact area",
            ),
            (26, "", Some("paintbrush.pointed"), "", "Redact brush"),
            (30, "", None, "", "Red"),
            (31, "", None, "", "Lime"),
            (32, "", None, "", "Cyan"),
            (33, "", None, "", "White"),
            (34, "Auto", None, "", "Automatic redaction tone"),
            (35, "Dark", None, "", "Dark redaction tone"),
            (36, "Light", None, "", "Light redaction tone"),
            (40, "", Some("arrow.uturn.backward"), "", "Undo (⌘Z)"),
            (42, "", Some("arrow.uturn.forward"), "", "Redo (⌘⇧Z)"),
            (
                41,
                "Copy",
                Some("square.on.square"),
                "↓",
                "Copy screenshot (↓)",
            ),
        ];
        let mut buttons = Vec::new();
        for (tag, title, symbol, key, label) in definitions {
            let style = match tag {
                30..=33 => ButtonStyle::Swatch(COLORS[(tag - 30) as usize]),
                41 => ButtonStyle::Primary,
                _ => ButtonStyle::Tool,
            };
            let button = Button::new(self.mtm(), title, "", symbol, key, style);
            button.setTag(tag);
            if matches!(tag,10|11|20..=26|30..=36) {
                button.setButtonType(NSButtonType::PushOnPushOff);
            }
            button.setToolTip(Some(&NSString::from_str(label)));
            button.setAccessibilityLabel(Some(&NSString::from_str(label)));
            // SAFETY: The target owns its buttons and implements this selector.
            unsafe {
                button.connect(self, sel!(toolbarAction:));
            }
            self.addSubview(&button);
            buttons.push(ButtonSlot { tag, button });
        }
        *self.ivars().buttons.borrow_mut() = buttons;
        self.position_buttons();
    }

    fn position_buttons(&self) {
        let bounds = self.bounds();
        let hud = hud_frame(bounds);
        let bar = toolbar_frame(bounds);
        let editing = self.ivars().state.borrow().editing;
        for chrome in self.ivars().chrome.borrow().iter() {
            chrome.setFrame(if chrome.ivars().toolbar { bar } else { hud });
        }
        for slot in self.ivars().buttons.borrow().iter() {
            let r = match slot.tag {
                10 => rect(hud.origin.x + 52., hud.origin.y + 8., 124., 32.),
                11 => rect(hud.origin.x + 180., hud.origin.y + 8., 124., 32.),
                12 => {
                    if editing {
                        rect(bar.origin.x + 664., bar.origin.y + 8., 32., 36.)
                    } else {
                        rect(hud.origin.x + 352., hud.origin.y + 8., 32., 32.)
                    }
                }
                20..=26 => rect(
                    bar.origin.x + 8. + (slot.tag - 20) as f64 * 40.,
                    bar.origin.y + 8.,
                    36.,
                    36.,
                ),
                30..=33 => rect(
                    bar.origin.x + 308. + (slot.tag - 30) as f64 * 32.,
                    bar.origin.y + 8.,
                    32.,
                    36.,
                ),
                34..=36 => rect(
                    bar.origin.x + 304. + (slot.tag - 34) as f64 * 46.,
                    bar.origin.y + 8.,
                    46.,
                    36.,
                ),
                40 => rect(bar.origin.x + 456., bar.origin.y + 8., 32., 36.),
                42 => rect(bar.origin.x + 492., bar.origin.y + 8., 32., 36.),
                41 => rect(bar.origin.x + 544., bar.origin.y + 8., 112., 36.),
                _ => NSRect::ZERO,
            };
            slot.button.setFrame(r);
        }
    }

    fn refresh_buttons(&self) {
        self.position_buttons();
        let s = self.ivars().state.borrow();
        for chrome in self.ivars().chrome.borrow().iter() {
            chrome.setHidden(if chrome.ivars().toolbar {
                !s.editing
            } else {
                s.editing
            });
            chrome.setNeedsDisplay(true);
        }
        for slot in self.ivars().buttons.borrow().iter() {
            let selected = match slot.tag {
                10 => s.mode == CaptureMode::Copy,
                11 => s.mode == CaptureMode::Markup,
                20..=26 => tool_index(s.editor.tool) == slot.tag - 20,
                30..=33 => s.editor.color_index == (slot.tag - 30) as usize,
                34..=36 => s.editor.redaction_tone == RedactionTone::ALL[(slot.tag - 34) as usize],
                _ => false,
            };
            if matches!(slot.tag,10|11|20..=26|30..=36) {
                slot.button.selected(selected);
                slot.button
                    .setAccessibilityValueDescription(Some(&NSString::from_str(if selected {
                        "Selected"
                    } else {
                        "Not selected"
                    })));
            }
            let hidden = if (30..=33).contains(&slot.tag) {
                !s.editing || s.editor.tool.is_pixelation()
            } else if (34..=36).contains(&slot.tag) {
                !s.editing || !s.editor.tool.is_pixelation()
            } else if slot.tag >= 20 {
                !s.editing
            } else {
                s.editing && slot.tag != 12
            };
            let entering = slot.button.isHidden() && !hidden;
            slot.button.setHidden(hidden);
            if entering {
                ui::fade(&slot.button, 0., 1.);
            }
            let enabled = match slot.tag {
                12 => true,
                40 => !s.busy && (!s.editor.marks.is_empty() || s.editor.current.is_some()),
                42 => !s.busy && !s.editor.redo.is_empty(),
                _ => !s.busy,
            };
            slot.button.setEnabled(enabled);
            NSView::setNeedsDisplay(&slot.button, true);
        }
    }

    fn draw_surface(&self) {
        let bounds = self.bounds();
        let s = self.ivars().state.borrow();
        if valid_rect(s.selection) {
            let border = NSBezierPath::bezierPathWithRect(s.selection);
            border.setLineWidth(1.0);
            color([1.0, 1.0, 1.0, if s.editing { 0.16 } else { 0.85 }]).setStroke();
            border.stroke();
            if s.editing {
                if let (Some(crop), Some(context)) =
                    (s.crop.as_ref(), NSGraphicsContext::currentContext())
                {
                    let cg = context.CGContext();
                    CGContext::save_g_state(Some(&cg));
                    CGContext::clip_to_rect(Some(&cg), s.selection);
                    CGContext::translate_ctm(Some(&cg), s.selection.origin.x, s.selection.origin.y);
                    CGContext::scale_ctm(
                        Some(&cg),
                        s.selection.size.width / crop.width() as f64,
                        s.selection.size.height / crop.height() as f64,
                    );
                    for mark in &s.editor.marks[s.cache.cached_count()..] {
                        mark.draw(&cg);
                    }
                    if let Some(mark) = s.editor.current.as_ref() {
                        mark.draw(&cg);
                    }
                    CGContext::restore_g_state(Some(&cg));
                }
            } else {
                draw_corners(s.selection);
                if let Some(pixels) =
                    geometry::pixel_rect(s.selection, bounds, image_size(&self.ivars().image))
                {
                    let y = if s.selection.origin.y > 90.0 {
                        s.selection.origin.y - 24.0
                    } else {
                        (s.selection.origin.y + s.selection.size.height + 20.0)
                            .min(bounds.size.height - 70.0)
                    };
                    pill_label(
                        &format!(
                            "{} × {}",
                            pixels.size.width as usize, pixels.size.height as usize
                        ),
                        NSPoint::new(
                            (s.selection.origin.x + s.selection.size.width / 2.0)
                                .clamp(72.0, (bounds.size.width - 72.0).max(72.0)),
                            y,
                        ),
                        12.0,
                        bounds.size.width - 32.,
                    );
                }
            }
        }
        if let Some(message) = s.message.as_deref() {
            pill_label(
                message,
                NSPoint::new(
                    bounds.size.width / 2.0,
                    if s.editing {
                        bounds.size.height - 116.0
                    } else {
                        bounds.size.height - 40.0
                    },
                ),
                12.0,
                (bounds.size.width - 32.).min(640.),
            );
        } else if !s.editing && s.start.is_none() {
            if s.busy {
                pill_label(
                    "Copying screenshot…",
                    NSPoint::new(bounds.size.width / 2., bounds.size.height - 40.),
                    12.,
                    bounds.size.width - 32.,
                );
            } else {
                capture_hint(bounds);
            }
        } else if let (true, Some(crop)) = (s.editing && !s.busy, s.crop.as_ref()) {
            draw_centered_text(
                &format!(
                    "{} × {} px   ·   Type or paste to add text",
                    crop.width(),
                    crop.height()
                ),
                NSPoint::new(bounds.size.width / 2.0, bounds.size.height - 104.0),
                12.0,
                MUTED,
            );
        }
    }
}

fn image_size(image: &Image) -> NSSize {
    NSSize::new(image.width() as f64, image.height() as f64)
}
fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}
fn valid_rect(r: NSRect) -> bool {
    r.size.width > 1.0 && r.size.height > 1.0
}
fn contains(r: NSRect, p: NSPoint) -> bool {
    p.x >= r.origin.x
        && p.y >= r.origin.y
        && p.x <= r.origin.x + r.size.width
        && p.y <= r.origin.y + r.size.height
}
fn hud_frame(b: NSRect) -> NSRect {
    rect((b.size.width - 400.0) / 2.0, 24.0, 400.0, 48.0)
}
fn toolbar_frame(b: NSRect) -> NSRect {
    rect(
        (b.size.width - 704.0) / 2.0,
        b.size.height - 76.0,
        704.0,
        52.0,
    )
}
fn tool_index(t: Tool) -> isize {
    match t {
        Tool::Pen => 0,
        Tool::Arrow => 1,
        Tool::Rectangle => 2,
        Tool::Highlighter => 3,
        Tool::Text => 4,
        Tool::PixelateArea => 5,
        Tool::PixelateBrush => 6,
    }
}
fn color(c: [f64; 4]) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(c[0], c[1], c[2], c[3])
}
fn attributes(size: f64, c: [f64; 4]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, 0.0);
    let tint = color(c);
    // SAFETY: Apple's attribute constants are immutable NSString objects. Font
    // and color are exactly the object types required for these two keys.
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*tint as &AnyObject],
        )
    }
}
fn draw_centered_text(text: &str, p: NSPoint, size: f64, c: [f64; 4]) {
    let text = NSString::from_str(text);
    let attrs = attributes(size, c);
    unsafe {
        let s = text.sizeWithAttributes(Some(&attrs));
        text.drawAtPoint_withAttributes(
            NSPoint::new(p.x - s.width / 2.0, p.y - s.height / 2.0),
            Some(&attrs),
        );
    }
}
fn capture_hint(bounds: NSRect) {
    let r = rect(
        (bounds.size.width - 432.) / 2.,
        bounds.size.height - 64.,
        432.,
        40.,
    );
    ui::panel(r, 12.);
    ui::text(
        "Drag to select",
        r.origin.x + 16.,
        r.origin.y + 12.,
        12.,
        false,
        ui::TEXT,
    );
    ui::keycap(
        "Space",
        rect(r.origin.x + 128., r.origin.y + 8., 48., 24.),
        false,
    );
    ui::text(
        "Full screen",
        r.origin.x + 184.,
        r.origin.y + 12.,
        12.,
        false,
        ui::SECONDARY,
    );
    ui::keycap(
        "Esc",
        rect(r.origin.x + 304., r.origin.y + 8., 32., 24.),
        false,
    );
    ui::text(
        "Cancel",
        r.origin.x + 344.,
        r.origin.y + 12.,
        12.,
        false,
        ui::SECONDARY,
    );
}
fn pill_label(text: &str, p: NSPoint, size: f64, max_width: f64) {
    let text = NSString::from_str(text);
    let attrs = attributes(size, TEXT);
    unsafe {
        let measured = text.boundingRectWithSize_options_attributes_context(
            NSSize::new((max_width - 32.).max(1.), 64.),
            NSStringDrawingOptions::UsesLineFragmentOrigin,
            Some(&attrs),
            None,
        );
        let s = NSSize::new(measured.size.width.ceil(), measured.size.height.ceil());
        ui::panel(
            rect(
                p.x - s.width / 2.0 - 16.0,
                p.y - s.height / 2.0 - 8.0,
                s.width + 32.0,
                s.height + 16.0,
            ),
            8.0,
        );
        text.drawInRect_withAttributes(
            rect(p.x - s.width / 2.0, p.y - s.height / 2.0, s.width, s.height),
            Some(&attrs),
        );
    }
}
fn draw_corners(r: NSRect) {
    let len = 16.0_f64.min(r.size.width.min(r.size.height) / 3.0);
    let path = NSBezierPath::bezierPath();
    for (x, y, dx, dy) in [
        (r.origin.x, r.origin.y, 1.0, 1.0),
        (r.origin.x + r.size.width, r.origin.y, -1.0, 1.0),
        (r.origin.x, r.origin.y + r.size.height, 1.0, -1.0),
        (
            r.origin.x + r.size.width,
            r.origin.y + r.size.height,
            -1.0,
            -1.0,
        ),
    ] {
        path.moveToPoint(NSPoint::new(x + dx * len, y));
        path.lineToPoint(NSPoint::new(x, y));
        path.lineToPoint(NSPoint::new(x, y + dy * len));
    }
    color(ACCENT).setStroke();
    path.setLineWidth(2.0);
    path.setLineCapStyle(NSLineCapStyle::Round);
    path.stroke();
}
fn lerp_rect(a: NSRect, b: NSRect, t: f64) -> NSRect {
    rect(
        a.origin.x + (b.origin.x - a.origin.x) * t,
        a.origin.y + (b.origin.y - a.origin.y) * t,
        a.size.width + (b.size.width - a.size.width) * t,
        a.size.height + (b.size.height - a.size.height) * t,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn animation_has_exact_endpoints() {
        let a = rect(12.0, 15.0, 80.0, 30.0);
        let b = rect(40.0, 90.0, 1200.0, 450.0);
        assert_eq!(lerp_rect(a, b, 0.0), a);
        assert_eq!(lerp_rect(a, b, 1.0), b);
    }
    #[test]
    fn canvas_clicks_exclude_outside_coordinates() {
        let r = rect(100.0, 80.0, 200.0, 100.0);
        assert!(contains(r, NSPoint::new(120.0, 90.0)));
        assert!(!contains(r, NSPoint::new(99.0, 90.0)));
        assert!(!contains(r, NSPoint::new(f64::NAN, 90.0)));
    }
}
