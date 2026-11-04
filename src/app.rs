//! Native application lifecycle and capture orchestration. AppKit objects never
//! leave the main thread. Only immutable images and encoded bytes cross queues.
use crate::{hotkeys::Hotkeys, preferences};
use dispatch2::{DispatchQoS, DispatchQueue, DispatchTime, GlobalQueueIdentifier};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    NSArray, NSData, NSNotification, NSNotificationCenter, NSNumber, NSPoint, NSRect, NSSize,
    NSString, NSURL, ns_string,
};
use objc2_service_management::SMAppService;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use still::{
    capture::{self, DisplayRequest, FrozenDisplay},
    imaging::{self, EncodedImage, Image, Mark},
    model::{CaptureMode, TrayAction, TrayState},
    overlay::{OverlayAction, OverlayView},
    tray_panel::PanelView,
    ui,
};

thread_local! { static DELEGATE: RefCell<Option<Retained<StillDelegate>>> = const { RefCell::new(None) }; }

/// Always defer actions so native callbacks cannot reenter a borrowed UI model.
pub fn enqueue(work: impl FnOnce(&StillDelegate) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        autoreleasepool(|_| {
            let delegate = DELEGATE.with(|slot| slot.borrow().clone());
            if let Some(delegate) = delegate {
                work(&delegate);
            }
        });
    });
}
fn after(duration: Duration, work: impl FnOnce(&StillDelegate) + Send + 'static) {
    if let Ok(time) = DispatchTime::try_from(duration) {
        let _ = DispatchQueue::main().after(time, move || enqueue(work));
    }
}

struct Surface {
    window: Retained<CaptureWindow>,
    view: Retained<OverlayView>,
    frame: NSRect,
}
pub struct State {
    model: TrayState,
    status: Option<Retained<NSStatusItem>>,
    popover: Option<Retained<NSPopover>>,
    panel: Option<Retained<PanelView>>,
    hotkeys: Option<Hotkeys>,
    hotkey_error: Option<String>,
    surfaces: Vec<Surface>,
    previous: Option<Retained<NSRunningApplication>>,
    last_external: Option<Retained<NSRunningApplication>>,
    pasteboard: Retained<NSPasteboard>,
    generation: u64,
    exporting: bool,
    demo: bool,
    ui_test: bool,
    toast: Option<Retained<NSPanel>>,
    toast_generation: u64,
    started: Instant,
}
impl State {
    fn new() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let ui_test = args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--ui-test"
                    | "--check-rendering"
                    | "--check-annotations"
                    | "--check-menu"
                    | "--check-panel"
                    | "--panel-demo"
            )
        });
        Self {
            model: preferences::load(),
            status: None,
            popover: None,
            panel: None,
            hotkeys: None,
            hotkey_error: None,
            surfaces: Vec::new(),
            previous: None,
            last_external: external_app(),
            pasteboard: if ui_test {
                NSPasteboard::pasteboardWithUniqueName()
            } else {
                NSPasteboard::generalPasteboard()
            },
            generation: 0,
            exporting: false,
            demo: ui_test || args.iter().any(|arg| arg == "--demo"),
            ui_test,
            toast: None,
            toast_generation: 0,
            started: Instant::now(),
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RefCell<State>]
    pub struct StillDelegate;
    unsafe impl NSObjectProtocol for StillDelegate {}
    unsafe impl NSApplicationDelegate for StillDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish(&self, _: &NSNotification) { enqueue(|app| app.launch()); }
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _: &NSApplication, _: bool) -> bool { enqueue(|app| app.show_menu()); false }
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_on_close(&self, _: &NSApplication) -> bool { false }
        #[unsafe(method(applicationDidBecomeActive:))]
        fn became_active(&self, _: &NSNotification) { enqueue(|app| app.refresh()); }
        #[unsafe(method(applicationDidChangeScreenParameters:))]
        fn displays_changed(&self, _: &NSNotification) { enqueue(|app| app.cancel()); }
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _: &NSNotification) { self.stop(); }
    }
    unsafe impl NSWindowDelegate for StillDelegate {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_key(&self, _: &NSNotification) { enqueue(|app| app.refresh()); }
    }
    impl StillDelegate {
        #[unsafe(method(openMenu:))]
        fn open_menu_action(&self, _: Option<&AnyObject>) { enqueue(|app| app.show_menu()); }
        #[unsafe(method(togglePanel:))]
        fn toggle_panel_action(&self,_:Option<&AnyObject>) {enqueue(|app| {
            let shown=app.ivars().borrow().popover.as_ref().is_some_and(|p|p.isShown());
            if shown {app.close_panel();}else{app.show_menu();}
        });}
        #[unsafe(method(captureCopy:))]
        fn copy_action(&self, _: Option<&AnyObject>) { enqueue(|app| app.start(CaptureMode::Copy)); }
        #[unsafe(method(captureMarkup:))]
        fn markup_action(&self, _: Option<&AnyObject>) { enqueue(|app| app.start(CaptureMode::Markup)); }
        #[unsafe(method(quitStill:))]
        fn quit_action(&self, _: Option<&AnyObject>) { NSApplication::sharedApplication(self.mtm()).terminate(None); }
        #[unsafe(method(workspaceChanged:))]
        fn workspace_changed(&self, _: &NSNotification) { enqueue(|app| {
            if let Some(external) = external_app() { app.ivars().borrow_mut().last_external = Some(external); }
        }); }
        #[unsafe(method(woke:))]
        fn woke(&self, _: &NSNotification) { enqueue(|app| { crate::hotkeys::reset_held(); app.refresh(); }); }
    }
);

define_class!(
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct CaptureWindow;
    impl CaptureWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_key(&self) -> bool { true }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_main(&self) -> bool { true }
    }
);

impl StillDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::new(State::new()));
        // SAFETY: NSObject initializer for our allocated subclass.
        unsafe { msg_send![super(this), init] }
    }

    fn launch(&self) {
        if std::env::args().any(|arg| arg == "--check-capture") {
            if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
                eprintln!("CAPTURE CHECK FAILED: Screen Recording access is required.");
                std::process::exit(1);
            }
            let started = Instant::now();
            capture::capture_displays(display_requests(self.mtm()), move |result| {
                enqueue(move |app| {
                    match result {
                        Ok(displays) => {
                            for display in displays {
                                println!(
                                    "CAPTURE CHECK: display {} captured {}×{} pixels",
                                    display.request.id,
                                    display.image.width(),
                                    display.image.height()
                                );
                            }
                            println!(
                                "CAPTURE CHECK: completed in {:.1} ms; no clipboard or disk image writes",
                                started.elapsed().as_secs_f64() * 1000.
                            );
                        }
                        Err(error) => {
                            eprintln!("CAPTURE CHECK FAILED: {error}");
                            std::process::exit(1);
                        }
                    }
                    NSApplication::sharedApplication(app.mtm()).terminate(None);
                })
            });
            return;
        }
        self.make_menus();
        let ui_test = self.ivars().borrow().ui_test;
        if !ui_test {
            let mut state = self.ivars().borrow_mut();
            match Hotkeys::new() {
                Ok(mut hotkeys) => {
                    state.hotkey_error = hotkeys.register(state.model.shortcut_style);
                    state.hotkeys = Some(hotkeys);
                }
                Err(error) => state.hotkey_error = Some(error),
            }
            if let Err(error) = preferences::setup_login() {
                state.model.error = Some(error);
            }
        }
        // SAFETY: this delegate remains retained for the entire run loop; all
        // selectors accept one NSNotification. Observers are removed on exit.
        unsafe {
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            center.addObserver_selector_name_object(
                self,
                sel!(workspaceChanged:),
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
            );
            center.addObserver_selector_name_object(
                self,
                sel!(woke:),
                Some(NSWorkspaceDidWakeNotification),
                None,
            );
        }
        self.refresh();
        let args: Vec<String> = std::env::args().collect();
        if args
            .iter()
            .any(|a| a == "--demo" || a == "--check-rendering" || a == "--check-annotations")
        {
            self.start(CaptureMode::Markup);
        }
        if args.iter().any(|arg| arg == "--check-menu") {
            let state = self.ivars().borrow();
            assert!(state.surfaces.is_empty());
            assert!(
                state
                    .status
                    .as_ref()
                    .is_some_and(|s| s.menu(self.mtm()).is_none())
            );
            assert!(state.popover.as_ref().is_some_and(|p| !p.isShown()));
            assert!(state.panel.is_some());
            println!(
                "MENU CHECK: native status item, attached capture/preferences panel, no standalone application window"
            );
            drop(state);
            NSApplication::sharedApplication(self.mtm()).terminate(None);
            return;
        }
        if args
            .iter()
            .any(|arg| arg == "--panel-demo" || arg == "--check-panel")
        {
            after(Duration::from_millis(150), |app| app.show_menu());
            if args.iter().any(|arg| arg == "--check-panel") {
                after(Duration::from_millis(500), |app| app.check_panel());
            }
        }
        if ui_test {
            println!(
                "UI CHECK: Rust Still ready; clipboard is private; login and global shortcuts unchanged"
            );
        }
    }

    fn make_menus(&self) {
        let mtm = self.mtm();
        let app_menu = NSMenu::new(mtm);
        self.menu_item(&app_menu, "Still menu", sel!(openMenu:), ",");
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.menu_item(&app_menu, "Quit Still", sel!(quitStill:), "q");
        let root = NSMenu::new(mtm);
        let item = NSMenuItem::new(mtm);
        item.setSubmenu(Some(&app_menu));
        root.addItem(&item);
        let edit = NSMenu::new(mtm);
        edit.setTitle(ns_string!("Edit"));
        for (title, action, key) in [
            ("Undo", sel!(undo:), "z"),
            ("Redo", sel!(redo:), "Z"),
            ("Cut", sel!(cut:), "x"),
            ("Copy", sel!(copy:), "c"),
            ("Paste", sel!(paste:), "v"),
            ("Select all", sel!(selectAll:), "a"),
        ] {
            // A nil target uses the first responder, including native text input.
            unsafe {
                edit.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(key),
                );
            }
        }
        let edit_item = NSMenuItem::new(mtm);
        edit_item.setTitle(ns_string!("Edit"));
        edit_item.setSubmenu(Some(&edit));
        root.addItem(&edit_item);
        NSApplication::sharedApplication(mtm).setMainMenu(Some(&root));
        let status = NSStatusBar::systemStatusBar().statusItemWithLength(-2.0);
        status.setAutosaveName(Some(ns_string!("Still")));
        if let Some(button) = status.button(mtm) {
            button.setImage(Some(&ui::status_image()));
            button.setToolTip(Some(ns_string!("Still — capture and copy")));
            button.setAccessibilityLabel(Some(ns_string!("Still screenshot menu")));
        }
        if let Some(button) = status.button(mtm) {
            unsafe {
                button.setTarget(Some(self));
                button.setAction(Some(sel!(togglePanel:)));
            }
        }
        let panel = PanelView::new(
            mtm,
            Rc::new(|action| enqueue(move |app| app.menu_action(action))),
        );
        let controller = NSViewController::new(mtm);
        controller.setView(&panel);
        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(!ui::reduce_motion());
        popover.setContentViewController(Some(&controller));
        popover.setContentSize(NSSize::new(
            still::tray_panel::WIDTH,
            still::tray_panel::HEIGHT,
        ));
        popover.setAppearance(
            NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref(),
        );
        {
            let mut state = self.ivars().borrow_mut();
            state.status = Some(status);
            state.panel = Some(panel);
            state.popover = Some(popover);
        }
        self.update_menu();
    }
    fn menu_item(
        &self,
        menu: &NSMenu,
        title: &str,
        action: Sel,
        key: &str,
    ) -> Retained<NSMenuItem> {
        unsafe {
            let item = menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            );
            item.setTarget(Some(self));
            item
        }
    }
    fn refresh(&self) {
        {
            let mut state = self.ivars().borrow_mut();
            preferences::refresh(&mut state.model);
        }
        self.update_menu();
    }
    fn update_menu(&self) {
        let state = self.ivars().borrow();
        if let Some(panel) = &state.panel {
            let mut model = state.model.clone();
            if model.error.is_none() {
                model.error = state.hotkey_error.clone();
            }
            panel.update(&model);
        }
    }
    fn close_panel(&self) {
        let popover = self.ivars().borrow().popover.clone();
        if let Some(popover) = popover {
            // Capture must never include the panel's dismissal animation.
            let animates = popover.animates();
            popover.setAnimates(false);
            popover.close();
            popover.setAnimates(animates);
        }
    }
    fn show_menu(&self) {
        if !self.ivars().borrow().surfaces.is_empty() {
            self.focus_capture();
            return;
        }
        if self.ivars().borrow().model.is_capturing {
            return;
        }
        self.refresh();
        let (popover, button, panel) = {
            let state = self.ivars().borrow();
            (
                state.popover.clone(),
                state.status.as_ref().and_then(|s| s.button(self.mtm())),
                state.panel.clone(),
            )
        };
        if let (Some(popover), Some(button)) = (popover, button) {
            if !popover.isShown() {
                NSApplication::sharedApplication(self.mtm()).activate();
                popover.showRelativeToRect_ofView_preferredEdge(
                    button.bounds(),
                    &button,
                    objc2_foundation::NSRectEdge::MinY,
                );
            }
            if let Some(panel) = panel
                && let Some(window) = panel.window()
            {
                window.setTitle(ns_string!("Still"));
                window.makeKeyWindow();
                window.makeFirstResponder(Some(&panel));
            }
        }
    }
    fn check_panel(&self) {
        let result = (|| -> Result<(), String> {
            let (popover, panel) = {
                let state = self.ivars().borrow();
                (
                    state.popover.clone().ok_or("No popover")?,
                    state.panel.clone().ok_or("No panel")?,
                )
            };
            if !popover.isShown() {
                return Err("Toolbar panel was not shown".into());
            }
            let window = panel.window().ok_or("Panel has no native window")?;
            if panel.frame().size.width != still::tray_panel::WIDTH
                || panel.frame().size.height != still::tray_panel::HEIGHT
            {
                return Err("Incorrect compact panel dimensions".into());
            }
            if window.styleMask().contains(NSWindowStyleMask::Titled) {
                return Err("Panel unexpectedly has an app title bar".into());
            }
            panel.displayIfNeeded();
            if let Some(bitmap) = panel.bitmapImageRepForCachingDisplayInRect(panel.bounds()) {
                panel.cacheDisplayInRect_toBitmapImageRep(panel.bounds(), &bitmap);
                if let Some(cg) = bitmap.CGImage() {
                    let image = Image::from_cg(cg.into())?;
                    std::fs::create_dir_all("artifacts").map_err(|e| e.to_string())?;
                    std::fs::write("artifacts/toolbar-panel.png", imaging::encode_png(&image)?)
                        .map_err(|e| e.to_string())?;
                }
            }
            self.close_panel();
            if popover.isShown() {
                return Err("Toolbar panel did not dismiss".into());
            }
            self.show_menu();
            if !popover.isShown() {
                return Err("Toolbar panel did not reopen".into());
            }
            self.close_panel();
            Ok(())
        })();
        match result {
            Ok(()) => println!(
                "PANEL CHECK: native status-icon anchor, compact untitled panel, dismiss/reopen passed"
            ),
            Err(error) => {
                eprintln!("PANEL CHECK FAILED: {error}");
                std::process::exit(1);
            }
        }
        NSApplication::sharedApplication(self.mtm()).terminate(None);
    }

    fn menu_action(&self, action: TrayAction) {
        match action {
            TrayAction::Dismiss => self.close_panel(),
            TrayAction::Capture(mode) => self.start(mode),
            TrayAction::RequestPermission => self.request_permission(),
            TrayAction::ToggleLogin => {
                let enabled = !self.ivars().borrow().model.launch_at_login;
                if self.ivars().borrow().ui_test {
                    self.ivars().borrow_mut().model.launch_at_login = enabled;
                    self.update_menu();
                } else {
                    self.ivars().borrow_mut().model.error = preferences::set_login(enabled).err();
                    self.refresh();
                }
                if self.ivars().borrow().model.login_needs_approval {
                    unsafe {
                        SMAppService::openSystemSettingsLoginItems();
                    }
                }
            }
            TrayAction::LoginSettings => unsafe {
                SMAppService::openSystemSettingsLoginItems();
            },
            TrayAction::SetShortcuts(style) => {
                {
                    let mut state = self.ivars().borrow_mut();
                    state.model.shortcut_style = style;
                    if !state.ui_test {
                        preferences::save_shortcuts(style);
                    }
                    state.hotkey_error =
                        state.hotkeys.as_mut().and_then(|keys| keys.register(style));
                }
                self.update_menu();
            }
            TrayAction::Quit => NSApplication::sharedApplication(self.mtm()).terminate(None),
        }
    }

    fn request_permission(&self) {
        self.close_panel();
        if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
            objc2_core_graphics::CGRequestScreenCaptureAccess();
        }
        self.refresh();
        if !self.ivars().borrow().model.screen_access
            && let Some(url) = NSURL::URLWithString(ns_string!(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            ))
        {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }

    pub fn hotkey(&self, id: u32) {
        let mode = if id == 2 {
            CaptureMode::Markup
        } else {
            CaptureMode::Copy
        };
        if !self.ivars().borrow().surfaces.is_empty() {
            self.focus_capture();
            if id != 3 {
                self.arrow(mode);
            }
        } else {
            self.start(mode);
        }
    }

    fn start(&self, mode: CaptureMode) {
        if !self.ivars().borrow().surfaces.is_empty() {
            self.arrow(mode);
            return;
        }
        if self.ivars().borrow().model.is_capturing {
            return;
        }
        self.close_panel();
        self.refresh();
        let demo = self.ivars().borrow().demo;
        if !demo && !self.ivars().borrow().model.screen_access {
            self.show_menu();
            self.request_permission();
            return;
        }
        let requests = display_requests(self.mtm());
        if requests.is_empty() {
            self.fail("No available display was found.".into());
            return;
        }
        let generation = {
            let mut state = self.ivars().borrow_mut();
            state.previous = external_app().or_else(|| state.last_external.clone());
            state.generation = state.generation.wrapping_add(1);
            state.model.is_capturing = true;
            state.model.error = None;
            state.started = Instant::now();
            state.generation
        };
        self.hide_toast();
        self.update_menu();
        if demo {
            // Same native overlay and image pipeline; only the input is synthetic.
            let request = requests
                .into_iter()
                .next()
                .expect("nonempty display requests");
            if std::env::args().any(|arg| arg == "--check-annotations") {
                let result = imaging::annotation_fixture(self.mtm(), request.width, request.height)
                    .map(|image| vec![FrozenDisplay { request, image }]);
                self.captured(generation, mode, result);
                return;
            }
            let worker = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
                DispatchQoS::UserInitiated,
            ));
            worker.exec_async(move || {
                let result = imaging::synthetic(request.width, request.height)
                    .map(|image| vec![FrozenDisplay { request, image }]);
                enqueue(move |app| app.captured(generation, mode, result));
            });
        } else {
            capture::capture_displays(requests, move |result| {
                enqueue(move |app| app.captured(generation, mode, result))
            });
        }
        after(Duration::from_secs(15), move |app| {
            let timed_out = {
                let state = app.ivars().borrow();
                state.generation == generation
                    && state.model.is_capturing
                    && state.surfaces.is_empty()
            };
            if timed_out {
                app.cancel();
                app.fail(
                    "Screen capture timed out. Check Screen Recording access and try again.".into(),
                );
            }
        });
    }

    fn captured(
        &self,
        generation: u64,
        mode: CaptureMode,
        result: Result<Vec<FrozenDisplay>, String>,
    ) {
        if self.ivars().borrow().generation != generation {
            return;
        }
        match result {
            Ok(displays) if !displays.is_empty() => self.present(displays, mode, generation),
            Ok(_) => {
                self.cancel();
                self.fail("No available display was found.".into());
            }
            Err(error) => {
                self.cancel();
                self.fail(format!("Couldn’t capture the screen. {error}"));
            }
        }
    }

    fn present(&self, displays: Vec<FrozenDisplay>, mode: CaptureMode, generation: u64) {
        let mtm = self.mtm();
        let mut surfaces = Vec::with_capacity(displays.len());
        for (index, display) in displays.into_iter().enumerate() {
            let frame = display.request.frame;
            let callback = Rc::new(move |action| {
                enqueue(move |app| app.overlay_action(generation, index, action))
            });
            let view = OverlayView::new(
                mtm,
                display.image,
                NSRect::new(NSPoint::new(0., 0.), frame.size),
                mode,
                callback,
            );
            let allocated = CaptureWindow::alloc(mtm).set_ivars(());
            // SAFETY: NSPanel designated initializer, then prevent close from
            // consuming the retained ownership held by Surface.
            let window: Retained<CaptureWindow> = unsafe {
                msg_send![super(allocated), initWithContentRect:frame,
                styleMask:NSWindowStyleMask::Borderless, backing:NSBackingStoreType::Buffered, defer:false]
            };
            unsafe {
                window.setReleasedWhenClosed(false);
            }
            window.setLevel(objc2_core_graphics::kCGScreenSaverWindowLevel as isize);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Stationary,
            );
            window.setOpaque(true);
            window.setHasShadow(false);
            window.setHidesOnDeactivate(false);
            window.setAcceptsMouseMovedEvents(true);
            window.setAnimationBehavior(NSWindowAnimationBehavior::None);
            window.setBackgroundColor(Some(&NSColor::blackColor()));
            window.setTitle(ns_string!("Still — Capture"));
            window.setContentView(Some(&view));
            surfaces.push(Surface {
                window,
                view,
                frame,
            });
        }
        self.ivars().borrow_mut().surfaces = surfaces;
        let windows: Vec<_> = self
            .ivars()
            .borrow()
            .surfaces
            .iter()
            .map(|s| s.window.clone())
            .collect();
        for window in windows {
            window.orderFrontRegardless();
        }
        self.focus_capture();
        if self.ivars().borrow().ui_test && std::env::args().any(|arg| arg == "--check-rendering") {
            let view = self
                .ivars()
                .borrow()
                .surfaces
                .first()
                .map(|s| s.view.clone());
            if let Some(view) = view {
                view.check_rendering();
            }
        }
        if std::env::args().any(|arg| arg == "--check-annotations") {
            let view = self
                .ivars()
                .borrow()
                .surfaces
                .first()
                .map(|s| s.view.clone());
            if let Some(view) = view {
                view.check_annotations_start();
            }
            after(Duration::from_millis(600), move |app| {
                let view = app
                    .ivars()
                    .borrow()
                    .surfaces
                    .first()
                    .map(|s| s.view.clone());
                if let Some(view) = view {
                    view.check_annotations_finish();
                } else {
                    eprintln!("ANNOTATION CHECK FAILED: capture was dismissed");
                    std::process::exit(1);
                }
            });
        }
        if self.ivars().borrow().ui_test {
            println!(
                "UI CHECK: overlay ready ({:.1} ms)",
                self.ivars().borrow().started.elapsed().as_secs_f64() * 1000.
            );
        }
    }

    fn focus_capture(&self) {
        let mouse = NSEvent::mouseLocation();
        let target = {
            let state = self.ivars().borrow();
            let surface = state
                .surfaces
                .iter()
                .find(|s| s.view.is_editing())
                .or_else(|| state.surfaces.iter().find(|s| contains(s.frame, mouse)))
                .or_else(|| state.surfaces.first());
            surface.map(|s| (s.window.clone(), s.view.clone()))
        };
        if let Some((window, view)) = target {
            NSApplication::sharedApplication(self.mtm()).activate();
            window.makeKeyAndOrderFront(None);
            view.restore_focus();
        }
    }

    fn arrow(&self, mode: CaptureMode) {
        let views: Vec<_> = self
            .ivars()
            .borrow()
            .surfaces
            .iter()
            .map(|s| s.view.clone())
            .collect();
        if let Some(view) = views.iter().find(|v| v.is_editing()) {
            view.handle_arrow(mode);
        } else {
            for view in views {
                view.set_mode(mode);
            }
        }
    }

    fn overlay_action(&self, generation: u64, index: usize, action: OverlayAction) {
        if self.ivars().borrow().generation != generation {
            return;
        }
        match action {
            OverlayAction::Cancel => self.cancel(),
            OverlayAction::AnnotationCheckFinished(result) => {
                self.cancel();
                match result {
                    Ok(()) => println!(
                        "ANNOTATION CHECK: native Unicode/paste input, text commit, pixelation masks, undo/redo, flattened export passed; general clipboard unchanged"
                    ),
                    Err(error) => {
                        eprintln!("ANNOTATION CHECK FAILED: {error}");
                        std::process::exit(1);
                    }
                }
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
            OverlayAction::RenderCheckFinished => {
                after(Duration::from_millis(100), move |app| {
                    let paused = app
                        .ivars()
                        .borrow()
                        .surfaces
                        .get(index)
                        .is_some_and(|s| s.view.frame_clock_is_paused());
                    println!("RENDER CHECK: idle_display_link_paused={paused}");
                    app.cancel();
                    if !paused {
                        std::process::exit(1);
                    }
                    NSApplication::sharedApplication(app.mtm()).terminate(None);
                });
            }
            OverlayAction::Copy { image, marks } => self.export(generation, index, image, marks),
            OverlayAction::Mode(mode) => {
                let views: Vec<_> = self
                    .ivars()
                    .borrow()
                    .surfaces
                    .iter()
                    .map(|s| s.view.clone())
                    .collect();
                for view in views {
                    view.set_mode(mode);
                }
            }
            OverlayAction::BeginMarkup => {
                let windows: Vec<_> = self
                    .ivars()
                    .borrow()
                    .surfaces
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .map(|(_, s)| s.window.clone())
                    .collect();
                for window in windows {
                    window.orderOut(None);
                }
            }
            OverlayAction::SelectionStarted => {
                let views: Vec<_> = self
                    .ivars()
                    .borrow()
                    .surfaces
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .map(|(_, s)| s.view.clone())
                    .collect();
                for view in views {
                    view.clear_selection();
                }
            }
        }
    }

    fn export(&self, generation: u64, index: usize, image: Image, marks: Vec<Mark>) {
        let view = {
            let mut state = self.ivars().borrow_mut();
            if state.exporting {
                return;
            }
            let view = state.surfaces.get(index).map(|s| s.view.clone());
            if view.is_none() {
                return;
            }
            state.exporting = true;
            view
        };
        if let Some(view) = view {
            view.set_busy(true);
        }
        let width = image.width();
        let height = image.height();
        let started = Instant::now();
        DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInitiated,
        ))
        .exec_async(move || {
            autoreleasepool(|_| {
                let result =
                    imaging::render(&image, &marks).and_then(|image| imaging::encode(&image));
                enqueue(move |app| app.exported(generation, index, width, height, started, result));
            });
        });
    }

    fn exported(
        &self,
        generation: u64,
        index: usize,
        width: usize,
        height: usize,
        started: Instant,
        result: Result<EncodedImage, String>,
    ) {
        if self.ivars().borrow().generation != generation {
            return;
        }
        self.ivars().borrow_mut().exporting = false;
        let result = result.and_then(|encoded| {
            let pasteboard = self.ivars().borrow().pasteboard.clone();
            write_clipboard(&pasteboard, encoded)
        });
        match result {
            Ok(()) => {
                let (frame, test) = {
                    let mut state = self.ivars().borrow_mut();
                    let persist = !state.ui_test;
                    preferences::did_copy(&mut state.model, persist);
                    (state.surfaces.get(index).map(|s| s.frame), state.ui_test)
                };
                if test {
                    println!(
                        "UI CHECK: copied {width}×{height}, PNG=true, TIFF=true, export_ms={:.1}",
                        started.elapsed().as_secs_f64() * 1000.
                    );
                }
                self.cancel();
                if let Some(frame) = frame {
                    self.show_toast(frame);
                }
            }
            Err(error) => {
                let view = self
                    .ivars()
                    .borrow()
                    .surfaces
                    .get(index)
                    .map(|s| s.view.clone());
                if let Some(view) = view {
                    view.set_busy(false);
                    view.show_error(&format!(
                        "Couldn’t copy. {error} Press ↓ to try again, or Esc to leave."
                    ));
                }
            }
        }
    }

    fn cancel(&self) {
        let (surfaces, previous) = {
            let mut state = self.ivars().borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.model.is_capturing = false;
            state.exporting = false;
            (std::mem::take(&mut state.surfaces), state.previous.take())
        };
        for surface in surfaces {
            surface.view.cancel_animation();
            surface.window.orderOut(None);
            surface.window.close();
        }
        NSCursor::arrowCursor().set();
        if let Some(previous) = previous
            && !previous.isTerminated()
        {
            previous.activateWithOptions(NSApplicationActivationOptions::empty());
        }
        self.update_menu();
    }

    fn fail(&self, error: String) {
        self.ivars().borrow_mut().model.error = Some(error);
        self.show_menu();
    }

    fn show_toast(&self, frame: NSRect) {
        self.hide_toast();
        let mtm = self.mtm();
        let rect = NSRect::new(
            NSPoint::new(
                frame.origin.x + frame.size.width / 2. - 116.,
                frame.origin.y + frame.size.height - 104.,
            ),
            NSSize::new(232., 48.),
        );
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe {
            panel.setReleasedWhenClosed(false);
        }
        panel.setLevel(3);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setIgnoresMouseEvents(true);
        let view = ui::CopiedView::new(mtm);
        panel.setContentView(Some(&view));
        panel.orderFrontRegardless();
        ui::fade(&view, 0., 1.);
        let generation = {
            let mut state = self.ivars().borrow_mut();
            state.toast = Some(panel);
            state.toast_generation
        };
        after(Duration::from_millis(1200), move |app| {
            let view = {
                let state = app.ivars().borrow();
                (state.toast_generation == generation)
                    .then(|| state.toast.as_ref().and_then(|toast| toast.contentView()))
                    .flatten()
            };
            if let Some(view) = view {
                ui::fade(&view, 1., 0.);
                after(Duration::from_secs_f64(ui::NOTIFICATION), move |app| {
                    if app.ivars().borrow().toast_generation == generation {
                        app.hide_toast();
                    }
                });
            }
        });
    }

    fn hide_toast(&self) {
        let toast = {
            let mut state = self.ivars().borrow_mut();
            state.toast_generation = state.toast_generation.wrapping_add(1);
            state.toast.take()
        };
        if let Some(toast) = toast {
            toast.orderOut(None);
            toast.close();
        }
    }

    fn stop(&self) {
        self.close_panel();
        self.cancel();
        self.hide_toast();
        // SAFETY: unregister the same observer that launch registered.
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(self);
            NSNotificationCenter::defaultCenter().removeObserver(self);
        }
        let mut state = self.ivars().borrow_mut();
        state.hotkeys = None;
        if state.ui_test {
            // A private named pasteboard has no effect on the user's clipboard.
            unsafe {
                let _: () = msg_send![&*state.pasteboard, releaseGlobally];
            }
        }
        if let Some(status) = state.status.take() {
            NSStatusBar::systemStatusBar().removeStatusItem(&status);
        }
    }
}

fn external_app() -> Option<Retained<NSRunningApplication>> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .filter(|app| app.processIdentifier() as u32 != std::process::id())
}
fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}
fn display_requests(mtm: MainThreadMarker) -> Vec<DisplayRequest> {
    NSScreen::screens(mtm)
        .iter()
        .filter_map(|screen| {
            let number = screen
                .deviceDescription()
                .objectForKey(ns_string!("NSScreenNumber"))?;
            let id = number.downcast_ref::<NSNumber>()?.unsignedIntValue();
            let frame = screen.frame();
            let scale = screen.backingScaleFactor();
            let width = (frame.size.width * scale).round();
            let height = (frame.size.height * scale).round();
            if width <= 0. || height <= 0. || !width.is_finite() || !height.is_finite() {
                return None;
            }
            Some(DisplayRequest {
                id,
                frame,
                width: width as usize,
                height: height as usize,
            })
        })
        .collect()
}

fn write_clipboard(pasteboard: &NSPasteboard, encoded: EncodedImage) -> Result<(), String> {
    // Build both representations before clearing the previous clipboard.
    let item = NSPasteboardItem::new();
    let png = NSData::with_bytes(&encoded.png);
    let tiff = NSData::with_bytes(&encoded.tiff);
    if !item.setData_forType(&png, ns_string!("public.png"))
        || !item.setData_forType(&tiff, ns_string!("public.tiff"))
    {
        return Err("Image representations could not be prepared.".into());
    }
    let object: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
    let objects = NSArray::from_slice(&[object]);
    pasteboard.clearContents();
    if !pasteboard.writeObjects(&objects) {
        return Err("The clipboard is busy.".into());
    }
    Ok(())
}

fn check_hotkeys() -> Result<(), String> {
    use still::model::ShortcutStyle;
    let mut first = Hotkeys::new()?;
    if let Some(error) = first.register(ShortcutStyle::Plain) {
        return Err(error);
    }
    first.verify_conflict()?;
    drop(first);
    let mut second = Hotkeys::new()?;
    if let Some(error) = second.register(ShortcutStyle::Plain) {
        return Err(format!("Hotkeys were not released: {error}"));
    }
    if let Some(error) = second.register(ShortcutStyle::Modified) {
        return Err(error);
    }
    Ok(())
}

fn check_clipboard() -> Result<(), String> {
    let image = imaging::synthetic(127, 83)?;
    let encoded = imaging::encode(&image)?;
    let expected_png = encoded.png.clone();
    let expected_tiff = encoded.tiff.clone();
    let pasteboard = NSPasteboard::pasteboardWithUniqueName();
    let result = (|| {
        write_clipboard(&pasteboard, encoded)?;
        for (kind, expected) in [("public.png", expected_png), ("public.tiff", expected_tiff)] {
            let actual = pasteboard
                .dataForType(&NSString::from_str(kind))
                .ok_or_else(|| format!("Missing clipboard type {kind}"))?;
            if actual.to_vec() != expected {
                return Err(format!("Clipboard changed {kind} bytes"));
            }
        }
        Ok(())
    })();
    // SAFETY: Only our unique private pasteboard is released, after all reads.
    unsafe {
        let _: () = msg_send![&*pasteboard, releaseGlobally];
    }
    result
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("Still must run on the macOS main thread");
    autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        if std::env::args().any(|arg| arg == "--check-hotkeys") {
            match check_hotkeys() {
                Ok(()) => println!(
                    "HOTKEY CHECK: registration, collision reporting, teardown, and modifier switch passed"
                ),
                Err(error) => {
                    eprintln!("HOTKEY CHECK FAILED: {error}");
                    std::process::exit(1);
                }
            }
            return;
        }
        if std::env::args().any(|arg| arg == "--check-clipboard") {
            match check_clipboard() {
                Ok(()) => println!(
                    "CLIPBOARD CHECK: PNG and TIFF byte-exact round trip passed; general clipboard unchanged"
                ),
                Err(error) => {
                    eprintln!("CLIPBOARD CHECK FAILED: {error}");
                    std::process::exit(1);
                }
            }
            return;
        }
        app.setActivationPolicy(if std::env::args().any(|a| a == "--ui-test") {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        });
        if let Some(appearance) =
            NSAppearance::appearanceNamed(ns_string!("NSAppearanceNameDarkAqua"))
        {
            app.setAppearance(Some(&appearance));
        }
        let delegate = StillDelegate::new(mtm);
        DELEGATE.with(|slot| *slot.borrow_mut() = Some(delegate.clone()));
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        app.run();
        app.setDelegate(None);
        DELEGATE.with(|slot| *slot.borrow_mut() = None);
    });
}
