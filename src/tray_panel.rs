//! The entire resting interface is a transient panel attached to the menu-bar icon.
use crate::{
    model::{CaptureMode, ShortcutStyle, TrayAction, TrayState},
    ui::{self, Button, ButtonStyle, rect},
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{NSRect, NSString};
use std::{cell::RefCell, rc::Rc};

pub const WIDTH: f64 = 340.;
pub const HEIGHT: f64 = 408.;
pub struct PanelIvars {
    callback: Rc<dyn Fn(TrayAction)>,
    buttons: RefCell<Vec<(isize, Retained<Button>)>>,
    status: Retained<NSTextField>,
    login: Retained<NSSwitch>,
}
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PanelIvars]
    pub struct PanelView;
    unsafe impl NSObjectProtocol for PanelView {}
    impl PanelView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self)->bool {true}
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self)->bool {true}
        #[unsafe(method(drawRect:))]
        fn draw(&self,_:NSRect) {
            ui::fill(self.bounds(),0.,ui::BACKGROUND);
            ui::logo(rect(20.,20.,36.,36.));
            ui::text("Still",68.,18.,22.,true,ui::TEXT);
            ui::separator(16.,264.,308.);
            ui::text("Launch at login",20.,284.,13.,false,ui::TEXT);
            ui::text("Global shortcuts",20.,328.,13.,false,ui::TEXT);
            ui::separator(16.,360.,308.);
            ui::text("Always in your menu bar",20.,376.,12.,false,ui::MUTED);
        }
        #[unsafe(method(panelAction:))]
        fn action(&self,sender:&NSControl) {self.activate(sender.tag());}
        #[unsafe(method(cancelOperation:))]
        fn cancel(&self,_:Option<&AnyObject>) {(self.ivars().callback)(TrayAction::Dismiss);}
        #[unsafe(method(keyDown:))]
        fn key(&self,event:&NSEvent) {
            if event.isARepeat() && matches!(event.keyCode(),125|126|36|76) {return;}
            match event.keyCode() {
                126=>self.activate(1),125=>self.activate(2),53=>self.activate(0),
                48=>{if let Some(window)=self.window(){
                    if event.modifierFlags().contains(NSEventModifierFlags::Shift){window.selectPreviousKeyView(None);}
                    else{window.selectNextKeyView(None);}
                }},
                _=>unsafe {let _:()=msg_send![super(self),keyDown:event];}
            }
        }
    }
);
impl PanelView {
    pub fn new(mtm: MainThreadMarker, callback: Rc<dyn Fn(TrayAction)>) -> Retained<Self> {
        let status = NSTextField::labelWithString(&NSString::from_str("Ready to capture"), mtm);
        status.setFrame(rect(68., 46., 228., 20.));
        status.setFont(Some(&NSFont::systemFontOfSize(12.)));
        status.setTextColor(Some(&ui::color(ui::SECONDARY)));
        status.setMaximumNumberOfLines(1);
        let login = NSSwitch::new(mtm);
        login.setFrame(rect(264., 276., 56., 32.));
        login.setTag(10);
        login.setAccessibilityLabel(Some(&NSString::from_str("Launch at login")));
        let this = Self::alloc(mtm).set_ivars(PanelIvars {
            callback,
            buttons: RefCell::new(Vec::new()),
            status,
            login,
        });
        let this: Retained<Self> =
            unsafe { msg_send![super(this),initWithFrame:rect(0.,0.,WIDTH,HEIGHT)] };
        this.setWantsLayer(true);
        this.setAccessibilityLabel(Some(&NSString::from_str("Still menu-bar panel")));
        this.addSubview(&this.ivars().status);
        this.addSubview(&this.ivars().login);
        unsafe {
            this.ivars().login.setTarget(Some(&this));
            this.ivars().login.setAction(Some(sel!(panelAction:)));
        }
        for (tag, title, subtitle, symbol, key, style, frame) in [
            (
                1,
                "Capture & copy",
                "Select an area, then carry on",
                Some("square.on.square"),
                "↑",
                ButtonStyle::Action,
                rect(16., 80., 308., 80.),
            ),
            (
                2,
                "Capture & annotate",
                "Draw, type, or dictate",
                Some("pencil.tip.crop.circle"),
                "↓",
                ButtonStyle::Action,
                rect(16., 168., 308., 80.),
            ),
            (
                11,
                "↑ ↓",
                "",
                None,
                "",
                ButtonStyle::Quiet,
                rect(164., 320., 56., 28.),
            ),
            (
                12,
                "⌃⌥ + ↑ ↓",
                "",
                None,
                "",
                ButtonStyle::Quiet,
                rect(224., 320., 100., 28.),
            ),
            (
                13,
                "",
                "",
                Some("record.circle"),
                "",
                ButtonStyle::Quiet,
                rect(300., 42., 24., 24.),
            ),
            (
                5,
                "Quit",
                "",
                None,
                "",
                ButtonStyle::Quiet,
                rect(268., 368., 56., 28.),
            ),
        ] {
            let button = Button::new(mtm, title, subtitle, symbol, key, style);
            button.setTag(tag);
            button.setFrame(frame);
            unsafe {
                button.connect(&this, sel!(panelAction:));
            }
            this.addSubview(&button);
            this.ivars().buttons.borrow_mut().push((tag, button));
        }
        this
    }
    fn activate(&self, tag: isize) {
        let action = match tag {
            1 => TrayAction::Capture(CaptureMode::Copy),
            2 => TrayAction::Capture(CaptureMode::Markup),
            10 => TrayAction::ToggleLogin,
            11 => TrayAction::SetShortcuts(ShortcutStyle::Plain),
            12 => TrayAction::SetShortcuts(ShortcutStyle::Modified),
            13 => TrayAction::RequestPermission,
            5 => TrayAction::Quit,
            _ => TrayAction::Dismiss,
        };
        (self.ivars().callback)(action);
    }
    pub fn update(&self, state: &TrayState) {
        let status = state.error.as_deref().unwrap_or(if state.is_capturing {
            "Preparing capture…"
        } else if state.screen_access {
            "Ready to capture"
        } else {
            "Allow Screen Recording to capture"
        });
        self.ivars()
            .status
            .setStringValue(&NSString::from_str(status));
        self.ivars()
            .status
            .setToolTip(Some(&NSString::from_str(status)));
        self.ivars().status.setTextColor(Some(&ui::color(
            if state.error.is_some() || !state.screen_access {
                ui::ERROR
            } else {
                ui::SECONDARY
            },
        )));
        self.ivars()
            .login
            .setState(if state.launch_at_login { 1 } else { 0 });
        for (tag, button) in self.ivars().buttons.borrow().iter() {
            match tag {
                1 | 2 => button.setEnabled(!state.is_capturing),
                11 => button.selected(state.shortcut_style == ShortcutStyle::Plain),
                12 => button.selected(state.shortcut_style == ShortcutStyle::Modified),
                13 => {
                    let title = if state.screen_access {
                        "Screen Recording enabled"
                    } else {
                        "Enable Screen Recording"
                    };
                    button.setAccessibilityLabel(Some(&NSString::from_str(title)));
                    button.setToolTip(Some(&NSString::from_str(title)));
                }
                _ => {}
            }
        }
    }
}
