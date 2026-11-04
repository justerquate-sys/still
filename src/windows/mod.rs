//! Native Windows application, implemented entirely in Rust.
mod native;
mod render;
mod text;
use crate::{
    coordinates::{Point, Rect, Size},
    drawing::{Stamp, Tool},
    editor::{COLORS, CacheRequest, Editor, RasterCacheState, RedactionTone},
    geometry,
    model::{CaptureMode, UpdateStatus},
    portable_image::{self as image, EncodedImage, Image},
    refresh, updater,
};
use render::{Renderer, r};
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, OnceLock, mpsc},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{
            Controls::WM_MOUSELEAVE, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};
const CLASS: PCWSTR = w!("Still.Native.Window");
const UPDATE_STARTUP_DELAY: Duration = Duration::from_secs(10);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
thread_local! {static EVENTS:RefCell<VecDeque<Event>>=const{RefCell::new(VecDeque::new())};}
#[derive(Clone, Copy)]
enum Event {
    Paint,
    Size(HWND, u32, u32),
    Move(HWND, Point),
    Leave(HWND),
    Down(HWND, Point),
    Up(HWND, Point),
    Key(u32, bool, bool, bool),
    AltKey(u32, bool, bool, bool),
    Character(u16),
    TextChanged,
    TextFinish(bool),
    TextDiscard,
    Hotkey(i32),
    Close(HWND),
    Tray(u32, u32),
    Deactivate(HWND, Point, u32),
    Dpi(HWND),
    Show,
    Display,
    RestoreTray,
    CaptureLost,
}
fn enqueue(event: Event) {
    EVENTS.with(|events| {
        let mut events = events.borrow_mut();
        if matches!(event, Event::Move(..)) && matches!(events.back(), Some(Event::Move(..))) {
            events.pop_back();
        }
        events.push_back(event);
    });
}
fn mouse(lparam: LPARAM) -> Point {
    Point::new(
        (lparam.0 as u16 as i16) as f64,
        ((lparam.0 >> 16) as u16 as i16) as f64,
    )
}
// SAFETY: Windows calls this with its documented WNDPROC ABI. It only queues
// owned scalar events; application state is never mutably borrowed by callbacks.
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if TASKBAR_CREATED.get().is_some_and(|id| *id == msg) {
            enqueue(Event::RestoreTray);
            return LRESULT(0);
        }
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
                enqueue(Event::Paint);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_SIZE => enqueue(Event::Size(
                hwnd,
                (lp.0 & 0xffff) as u32,
                ((lp.0 >> 16) & 0xffff) as u32,
            )),
            WM_MOUSEMOVE => {
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    ..Default::default()
                };
                let _ = TrackMouseEvent(&mut tracking);
                enqueue(Event::Move(hwnd, mouse(lp)));
            }
            WM_MOUSELEAVE => enqueue(Event::Leave(hwnd)),
            WM_LBUTTONDOWN => enqueue(Event::Down(hwnd, mouse(lp))),
            WM_LBUTTONUP => enqueue(Event::Up(hwnd, mouse(lp))),
            WM_CAPTURECHANGED => enqueue(Event::CaptureLost),
            WM_KEYDOWN => {
                let event = if GetKeyState(VK_MENU.0 as i32) < 0 {
                    Event::AltKey
                } else {
                    Event::Key
                };
                enqueue(event(
                    wp.0 as u32,
                    (lp.0 & (1 << 30)) != 0,
                    GetKeyState(VK_CONTROL.0 as i32) < 0,
                    GetKeyState(VK_SHIFT.0 as i32) < 0,
                ));
            }
            WM_SYSKEYDOWN if wp.0 == VK_UP.0 as usize || wp.0 == VK_DOWN.0 as usize => {
                enqueue(Event::AltKey(
                    wp.0 as u32,
                    (lp.0 & (1 << 30)) != 0,
                    GetKeyState(VK_CONTROL.0 as i32) < 0,
                    GetKeyState(VK_SHIFT.0 as i32) < 0,
                ));
            }
            WM_CHAR => enqueue(Event::Character(wp.0 as u16)),
            WM_COMMAND if (wp.0 >> 16) as u32 == EN_CHANGE => enqueue(Event::TextChanged),
            WM_CTLCOLOREDIT => {
                let dc = HDC(wp.0 as *mut _);
                SetBkColor(dc, COLORREF(0x00131211));
                SetTextColor(dc, COLORREF(text::INK.with(|c| c.get())));
                SetDCBrushColor(dc, COLORREF(0x00131211));
                return LRESULT(GetStockObject(DC_BRUSH).0 as isize);
            }
            WM_HOTKEY => enqueue(Event::Hotkey(wp.0 as i32)),
            WM_CLOSE => enqueue(Event::Close(hwnd)),
            WM_ACTIVATE if wp.0 & 0xffff == WA_INACTIVE as usize => {
                let point = native::cursor();
                enqueue(Event::Deactivate(
                    hwnd,
                    Point::new(point.x as f64, point.y as f64),
                    GetMessageTime() as u32,
                ));
            }
            WM_CANCELMODE => {
                enqueue(Event::Deactivate(hwnd, Point::ZERO, 0));
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            WM_DISPLAYCHANGE => enqueue(Event::Display),
            WM_SETTINGCHANGE if wp.0 == SPI_SETWORKAREA.0 as usize => enqueue(Event::Dpi(hwnd)),
            WM_DPICHANGED => {
                if GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_POPUP.0 == 0 {
                    let rect = *(lp.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                enqueue(Event::Dpi(hwnd));
            }
            native::TRAY => enqueue(Event::Tray(lp.0 as u32 & 0xffff, GetMessageTime() as u32)),
            native::SHOW => enqueue(Event::Show),
            native::WAKE => {}
            WM_SETCURSOR if lp.0 & 0xffff == HTCLIENT as isize => return LRESULT(1),
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        LRESULT(0)
    }
}
struct Window {
    handle: HWND,
    renderer: Option<Renderer>,
    size: Size,
    buttons: Vec<Button>,
}
impl Window {
    fn new(title: &str, bounds: Rect, popup: bool) -> Result<Self, String> {
        unsafe {
            let title = native::wide(title);
            let style = if popup {
                WS_POPUP
            } else {
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
            };
            let handle = CreateWindowExW(
                if popup {
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST
                } else {
                    WINDOW_EX_STYLE(0)
                },
                CLASS,
                PCWSTR(title.as_ptr()),
                style,
                bounds.origin.x as i32,
                bounds.origin.y as i32,
                bounds.size.width as i32,
                bounds.size.height as i32,
                None,
                None,
                Some(GetModuleHandleW(None).map_err(|e| e.to_string())?.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let mut client = RECT::default();
            let _ = GetClientRect(handle, &mut client);
            if !popup {
                let dark = windows::core::BOOL(1);
                let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                    handle,
                    windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE,
                    (&dark as *const windows::core::BOOL).cast(),
                    std::mem::size_of_val(&dark) as u32,
                );
            }
            Ok(Self {
                handle,
                renderer: None,
                size: Size::new(client.right as f64, client.bottom as f64),
                buttons: Vec::new(),
            })
        }
    }
    fn show(&self, activate: bool) {
        unsafe {
            let _ = ShowWindow(
                self.handle,
                if activate { SW_SHOW } else { SW_SHOWNOACTIVATE },
            );
            if activate {
                let _ = SetForegroundWindow(self.handle);
                let _ = SetFocus(Some(self.handle));
            }
        }
    }
    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.handle, SW_HIDE);
        }
    }
    fn renderer(&mut self) -> Result<&mut Renderer, String> {
        if self.renderer.is_none() {
            self.renderer = Some(
                Renderer::new(self.handle, self.size.width as u32, self.size.height as u32)
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(self.renderer.as_mut().unwrap())
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.renderer = None;
        unsafe {
            let _ = DestroyWindow(self.handle);
        }
    }
}
#[derive(Clone)]
struct Button {
    id: i32,
    rect: Rect,
    label: String,
    subtitle: String,
    key: String,
    primary: bool,
    selected: bool,
    enabled: bool,
    swatch: Option<u32>,
}
impl Button {
    fn new(id: i32, rect: Rect, label: &str) -> Self {
        Self {
            id,
            rect,
            label: label.into(),
            subtitle: String::new(),
            key: String::new(),
            primary: false,
            selected: false,
            enabled: true,
            swatch: None,
        }
    }
}
struct Zoom {
    started: Instant,
    from: Rect,
    to: Rect,
}
#[derive(Default)]
struct PanelState {
    visible: bool,
    tray_press: Option<TrayPress>,
}
#[derive(Clone, Copy)]
enum TrayPress {
    // Deactivation can precede the shell's notification of the same mouse down.
    Deactivated(u32),
    Down(bool),
}
impl PanelState {
    fn deactivate(&mut self, on_tray: bool, time: u32) {
        if self.visible && on_tray {
            self.tray_press = Some(TrayPress::Deactivated(time));
        }
        self.visible = false;
    }
    fn tray_down(&mut self, time: u32) {
        let close = self.visible
            || matches!(self.tray_press, Some(TrayPress::Deactivated(at))
                if time.wrapping_sub(at) <= 250);
        self.tray_press = Some(TrayPress::Down(close));
    }
    fn tray_release(&mut self, time: u32) -> bool {
        let close = match self.tray_press.take() {
            Some(TrayPress::Down(close)) => close,
            Some(TrayPress::Deactivated(at)) => time.wrapping_sub(at) <= 250,
            None => false,
        };
        !self.visible && !close
    }
}
struct Capture {
    brush_cursor: Option<native::BrushCursor>,
    input: Option<text::Input>,
    text_origin: Point,
    window: Window,
    original: Image,
    desktop: Rect,
    display: Rect,
    mode: CaptureMode,
    selection: Rect,
    start: Option<Point>,
    crop: Option<Image>,
    redaction_selection: Rect,
    raster: Option<Image>,
    cache: RasterCacheState,
    editor: Editor,
    zoom: Option<Zoom>,
    busy: bool,
    message: Option<String>,
    scale: f64,
}

fn redaction_palette(image: &Image, selection: Rect, bounds: Rect) -> crate::redaction::Palette {
    let Some(crop) = geometry::pixel_rect(
        selection,
        bounds,
        Size::new(image.width() as f64, image.height() as f64),
    ) else {
        return crate::redaction::Palette::DARK;
    };
    let mut samples = Vec::with_capacity(1024);
    for region in crate::drawing::redaction_sample_regions(image.width(), image.height(), crop) {
        let width = region.size.width as usize;
        let height = region.size.height as usize;
        let columns = width.min(32);
        let rows = height.min(32);
        for y in 0..rows {
            for x in 0..columns {
                let x = region.origin.x as usize + (2 * x + 1) * width / (2 * columns);
                let y = region.origin.y as usize + (2 * y + 1) * height / (2 * rows);
                let offset = (y * image.width() + x) * 4;
                samples.push(image.pixels()[offset..offset + 4].try_into().unwrap());
            }
        }
    }
    crate::redaction::palette_from_samples(&samples)
}

enum Reply {
    Mosaic(u64, u64, Result<Arc<Stamp>, String>),
    Captured(u64, Result<(Image, Rect, Rect, f64), String>),
    Exported(u64, Result<EncodedImage, String>),
    Raster(u64, CacheRequest, Result<Image, String>),
    UpdateChecked(Result<Option<updater::PreparedUpdate>, String>),
    UpdateApplyReady(Result<updater::ApplyProcess, String>),
}
struct Updates {
    config: Option<Arc<updater::Config>>,
    status: UpdateStatus,
    next_check: Option<Instant>,
    checking: bool,
    prepared: Option<Arc<updater::PreparedUpdate>>,
    applying: bool,
    apply: Option<updater::ApplyProcess>,
    exit_for_update: bool,
}
impl Updates {
    fn disabled() -> Self {
        Self {
            config: None,
            status: UpdateStatus::NotConfigured,
            next_check: None,
            checking: false,
            prepared: None,
            applying: false,
            apply: None,
            exit_for_update: false,
        }
    }
    fn new(ui_test: bool) -> Self {
        let mut updates = Self::disabled();
        if !ui_test {
            match updater::Config::embedded() {
                Ok(Some(config)) => {
                    updates.config = Some(Arc::new(config));
                    updates.status = UpdateStatus::Idle;
                    updates.next_check = Some(Instant::now() + UPDATE_STARTUP_DELAY);
                }
                Ok(None) => {}
                Err(error) => {
                    updates.status = UpdateStatus::Failed(error.to_string());
                    updates.next_check = Some(Instant::now() + UPDATE_CHECK_INTERVAL);
                }
            }
        }
        updates
    }
}
struct App {
    hotkeys: Option<native::Hotkeys>,
    tray: Option<native::Tray>,
    host: Window,
    panel: PanelState,
    panel_scale: f64,
    capture: Option<Capture>,
    toast: Option<(Window, Instant)>,
    previous: HWND,
    receiver: mpsc::Receiver<Reply>,
    sender: mpsc::Sender<Reply>,
    generation: u64,
    image_workers: usize,
    capturing: bool,
    pending_mode: CaptureMode,
    deadline: Option<Instant>,
    ui_test: bool,
    modified: bool,
    login: bool,
    error: Option<String>,
    shortcut_error: Option<String>,
    updates: Updates,
    dirty: bool,
    next_frame: Instant,
    rate: f64,
    hover: Option<i32>,
    focus: Option<i32>,
    pressed: Option<i32>,
    quit: bool,
}
fn contains(r: Rect, p: Point) -> bool {
    p.x >= r.origin.x
        && p.y >= r.origin.y
        && p.x < r.origin.x + r.size.width
        && p.y < r.origin.y + r.size.height
}
fn relative(r: Rect, origin: Point) -> Rect {
    Rect::new(
        Point::new(r.origin.x - origin.x, r.origin.y - origin.y),
        r.size,
    )
}
fn interpolate(a: Rect, b: Rect, t: f64) -> Rect {
    r(
        a.origin.x + (b.origin.x - a.origin.x) * t,
        a.origin.y + (b.origin.y - a.origin.y) * t,
        a.size.width + (b.size.width - a.size.width) * t,
        a.size.height + (b.size.height - a.size.height) * t,
    )
}
/// Registered hotkeys own their matching chord. Bare arrows remain useful in
/// the focused panel/overlay when global shortcuts use Ctrl+Alt or are unavailable.
fn local_capture_action(
    key: u32,
    repeated: bool,
    control: bool,
    shift: bool,
    alt: bool,
    modified: bool,
    registered: bool,
) -> Option<i32> {
    let id = if key == VK_UP.0 as u32 {
        1
    } else if key == VK_DOWN.0 as u32 {
        2
    } else {
        return None;
    };
    let global_chord = !shift
        && if modified {
            control && alt
        } else {
            !control && !alt
        };
    let local_chord = !control && !shift && !alt;
    (!repeated && (local_chord || global_chord) && !(global_chord && registered)).then_some(id)
}
fn update_idle(
    capture: bool,
    acquiring: bool,
    image_workers: usize,
    popup: bool,
    quit: bool,
) -> bool {
    !capture && !acquiring && image_workers == 0 && !popup && !quit
}
fn update_action_enabled(status: &UpdateStatus) -> bool {
    status.can_check()
}
impl App {
    fn new() -> Result<Self, String> {
        let ui_test = std::env::args().any(|s| s == "--ui-test");
        let point = native::cursor();
        let (_, rate) = native::monitor(point);
        let host = Window::new("Still", r(0., 0., 1., 1.), true)?;
        native::style_panel(host.handle);
        let modified = native::modified();
        let mut hotkeys = native::Hotkeys::new(host.handle);
        let shortcut_error = if ui_test {
            None
        } else {
            hotkeys.register(modified).err()
        };
        let tray = native::Tray::new(host.handle).map_err(|e| e.to_string())?;
        unsafe {
            SendMessageW(
                host.handle,
                WM_SETICON,
                Some(WPARAM(1)),
                Some(LPARAM(tray.icon().0 as isize)),
            );
        }
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            hotkeys: Some(hotkeys),
            tray: Some(tray),
            host,
            panel: PanelState::default(),
            panel_scale: 1.,
            capture: None,
            toast: None,
            previous: HWND::default(),
            receiver,
            sender,
            generation: 0,
            image_workers: 0,
            capturing: false,
            pending_mode: CaptureMode::Copy,
            deadline: None,
            ui_test,
            modified,
            login: native::login_enabled(),
            error: None,
            shortcut_error,
            updates: Updates::new(ui_test),
            dirty: true,
            next_frame: Instant::now(),
            rate,
            hover: None,
            focus: None,
            pressed: None,
            quit: false,
        })
    }
    fn show_menu(&mut self) {
        if let Some(c) = &self.capture {
            c.window.show(true);
            return;
        }
        if self.capturing {
            return;
        }
        self.remember_foreground();
        self.position_panel();
        self.panel.visible = true;
        self.panel.tray_press = None;
        self.hover = None;
        self.focus = None;
        self.pressed = None;
        self.dirty = true;
        // Render before showing so neither stale preferences nor an empty frame
        // flashes. The resting panel otherwise draws only in response to events.
        if let Err(error) = self.draw_panel() {
            self.error = Some(error);
            self.host.renderer = None;
        }
        self.host.show(true);
    }
    fn remember_foreground(&mut self) {
        let current = unsafe { GetForegroundWindow() };
        if !current.is_invalid()
            && current != self.host.handle
            && !self
                .capture
                .as_ref()
                .is_some_and(|c| c.window.handle == current)
            && !self
                .toast
                .as_ref()
                .is_some_and(|(toast, _)| toast.handle == current)
        {
            self.previous = current;
        }
    }
    fn position_panel(&mut self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let (bounds, scale) = tray.panel_placement();
        self.panel_scale = scale;
        self.host.size = bounds.size;
        self.host.buttons = panel_buttons(self.modified, self.login, scale, &self.updates.status);
        if let Some(renderer) = self.host.renderer.as_mut()
            && renderer
                .resize(bounds.size.width as u32, bounds.size.height as u32)
                .is_err()
        {
            self.host.renderer = None;
        }
        unsafe {
            let _ = SetWindowPos(
                self.host.handle,
                Some(HWND_TOPMOST),
                bounds.origin.x as i32,
                bounds.origin.y as i32,
                bounds.size.width as i32,
                bounds.size.height as i32,
                SWP_NOACTIVATE,
            );
        }
        self.dirty = true;
    }
    fn dismiss_panel(&mut self, restore: bool) {
        self.panel.visible = false;
        self.host.hide();
        self.host.buttons.clear();
        self.focus = None;
        self.hover = None;
        self.pressed = None;
        unsafe {
            if GetCapture() == self.host.handle {
                let _ = ReleaseCapture();
            }
            if restore && !self.previous.is_invalid() {
                let _ = SetForegroundWindow(self.previous);
            }
        }
    }
    fn fallback_menu(&mut self) {
        if self.capture.is_some() || self.capturing {
            return;
        }
        self.remember_foreground();
        self.dismiss_panel(false);
        self.panel.tray_press = None;
        if let Some(tray) = &self.tray {
            let id = tray.menu(
                self.modified,
                self.login,
                self.error.as_deref().or(self.shortcut_error.as_deref()),
            );
            // The native fallback also foregrounds the hidden host. Capture must
            // retain the external app remembered before opening that menu.
            self.activate(id as i32);
            if self.capture.is_none() && !self.capturing && !self.quit {
                self.dismiss_panel(unsafe { GetForegroundWindow() } == self.host.handle);
            }
        }
    }
    fn start(&mut self, mode: CaptureMode) {
        if let Some(c) = self.capture.as_mut() {
            if c.busy {
                return;
            }
            if c.crop.is_some() {
                self.export();
            } else {
                c.mode = mode;
                self.dirty = true;
            }
            return;
        }
        if self.capturing {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.error = None;
        self.capturing = true;
        self.deadline = Some(Instant::now() + Duration::from_secs(15));
        self.toast = None;
        self.remember_foreground();
        self.dismiss_panel(true);
        self.panel.tray_press = None;
        let test = self.ui_test;
        let bounds = native::desktop();
        let (display, rate) = native::monitor(native::cursor());
        self.rate = rate;
        // Carry the mode independently of worker timing through the active request.
        self.pending_mode = mode;
        self.focus = None;
        self.hover = None;
        self.pressed = None;
        self.image_workers += 1;
        native::worker(self.sender.clone(), self.host.handle, move || {
            native::flush_desktop();
            Reply::Captured(
                generation,
                (if test {
                    image::synthetic(bounds.size.width as usize, bounds.size.height as usize)
                } else {
                    native::capture(bounds)
                })
                .map(|image| (image, bounds, display, rate)),
            )
        });
    }
    fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.capture = None;
        self.capturing = false;
        self.deadline = None;
        self.focus = None;
        self.hover = None;
        self.pressed = None;
        unsafe {
            let _ = ReleaseCapture();
            if !self.previous.is_invalid() {
                let _ = SetForegroundWindow(self.previous);
            }
            if let Ok(cursor) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(cursor));
            }
        }
        self.dirty = true;
    }
    fn error(&mut self, message: String) {
        self.cancel();
        self.error = Some(message);
        self.show_menu();
    }
    fn replies(&mut self) {
        while let Ok(reply) = self.receiver.try_recv() {
            // A canceled capture invalidates its generation, but its native/image
            // workers must still finish before an automatic update may restart us.
            if matches!(
                &reply,
                Reply::Mosaic(..) | Reply::Captured(..) | Reply::Exported(..) | Reply::Raster(..)
            ) {
                self.image_workers = self.image_workers.saturating_sub(1);
            }
            match reply {
                Reply::UpdateChecked(result) if self.updates.checking => {
                    self.updates.checking = false;
                    self.updates.next_check = Some(Instant::now() + UPDATE_CHECK_INTERVAL);
                    match result {
                        Ok(Some(prepared)) => {
                            self.updates.status = UpdateStatus::Ready(prepared.version_string());
                            self.updates.prepared = Some(Arc::new(prepared));
                            self.updates.next_check = None;
                        }
                        Ok(None) => self.updates.status = UpdateStatus::UpToDate,
                        Err(error) => self.updates.status = UpdateStatus::Failed(error),
                    }
                    self.dirty |= self.panel.visible;
                }
                Reply::UpdateApplyReady(result) if self.updates.applying => {
                    self.updates.applying = false;
                    match result {
                        Ok(guard) => self.updates.apply = Some(guard),
                        Err(error) => {
                            self.updates.prepared = None;
                            self.updates.status = UpdateStatus::Failed(error);
                            self.updates.next_check = Some(Instant::now() + UPDATE_CHECK_INTERVAL);
                        }
                    }
                    self.dirty |= self.panel.visible;
                }
                Reply::Mosaic(generation, revision, result) if generation == self.generation => {
                    let again = if let Some(c) = self.capture.as_mut() {
                        let error = result.as_ref().err().cloned();
                        let stale = c.editor.complete_pixelation(revision, result.ok());
                        if !stale {
                            if let Some(error) = error {
                                c.message = Some(error);
                            } else if c.message.as_deref() == Some("Preparing redaction…") {
                                c.message = None;
                            }
                        }
                        self.dirty = true;
                        stale && c.editor.tool.is_pixelation()
                    } else {
                        false
                    };
                    if again {
                        self.warm_pixelation();
                    }
                }
                Reply::Captured(generation, result) if generation == self.generation => {
                    self.capturing = false;
                    self.deadline = None;
                    match result {
                        Ok((image, bounds, display, rate)) => {
                            match Window::new("Still — Capture", bounds, true) {
                                Ok(window) => {
                                    let scale = native::display_scale(native::cursor())
                                        .min(display.size.width / 880.);
                                    let mode = self.pending_mode;
                                    self.focus = None;
                                    self.capture = Some(Capture {
                                        brush_cursor: None,
                                        input: None,
                                        text_origin: Point::new(16., 16.),
                                        window,
                                        original: image,
                                        desktop: bounds,
                                        display: relative(display, bounds.origin),
                                        mode,
                                        selection: Rect::ZERO,
                                        start: None,
                                        crop: None,
                                        redaction_selection: Rect::ZERO,
                                        raster: None,
                                        cache: RasterCacheState::default(),
                                        editor: Editor::default(),
                                        zoom: None,
                                        busy: false,
                                        message: None,
                                        scale,
                                    });
                                    self.rate = rate;
                                    self.capture.as_ref().unwrap().window.show(true);
                                    self.dirty = true;
                                }
                                Err(error) => self.error(error),
                            }
                        }
                        Err(error) => self.error(error),
                    }
                }
                Reply::Exported(generation, result) if generation == self.generation => {
                    self.deadline = None;
                    let result = result.and_then(|encoded| {
                        if self.ui_test {
                            if encoded.png.is_empty() || encoded.dibv5.len() < 124 {
                                Err("Private clipboard validation failed".into())
                            } else {
                                Ok(())
                            }
                        } else {
                            native::copy(self.host.handle, &encoded)
                        }
                    });
                    match result {
                        Ok(()) => {
                            let display = self.capture.as_ref().map(|c| {
                                Rect::new(
                                    Point::new(
                                        c.display.origin.x + c.desktop.origin.x,
                                        c.display.origin.y + c.desktop.origin.y,
                                    ),
                                    c.display.size,
                                )
                            });
                            self.cancel();
                            if let Some(display) = display {
                                self.copied(display);
                            }
                        }
                        Err(error) => {
                            if let Some(c) = self.capture.as_mut() {
                                c.busy = false;
                                refresh_input(c, true);
                                c.message = Some(error);
                                if c.crop.is_some() && c.raster.is_none() {
                                    c.raster = c.crop.clone();
                                }
                            }
                            self.dirty = true;
                        }
                    }
                }
                Reply::Raster(generation, request, result) if generation == self.generation => {
                    if let Some(c) = self.capture.as_mut() {
                        if c.cache.complete(request, result.is_ok())
                            && let Ok(image) = result
                        {
                            c.raster = Some(image);
                        }
                        self.dirty = true;
                    }
                    self.schedule_raster();
                }
                _ => {}
            }
        }
    }
    fn check_for_updates(&mut self) {
        if self.ui_test
            || self.quit
            || self.updates.checking
            || self.updates.prepared.is_some()
            || self.updates.applying
            || self.updates.apply.is_some()
        {
            return;
        }
        if self.updates.config.is_none() {
            match updater::Config::embedded() {
                Ok(Some(config)) => self.updates.config = Some(Arc::new(config)),
                Ok(None) => {
                    self.updates.status = UpdateStatus::NotConfigured;
                    self.updates.next_check = None;
                    self.dirty |= self.panel.visible;
                    return;
                }
                Err(error) => {
                    self.updates.status = UpdateStatus::Failed(error.to_string());
                    self.updates.next_check = Some(Instant::now() + UPDATE_CHECK_INTERVAL);
                    self.dirty |= self.panel.visible;
                    return;
                }
            }
        }
        let config = Arc::clone(self.updates.config.as_ref().unwrap());
        self.updates.checking = true;
        self.updates.status = UpdateStatus::Checking;
        self.updates.next_check = None;
        self.dirty |= self.panel.visible;
        native::worker(self.sender.clone(), self.host.handle, move || {
            Reply::UpdateChecked(
                updater::InstallContext::current()
                    .and_then(|context| updater::check_and_stage(&config, &context))
                    .map_err(|error| error.to_string()),
            )
        });
    }
    fn poll_updates(&mut self, now: Instant) {
        if self.ui_test || self.quit {
            return;
        }
        if self
            .updates
            .next_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.check_for_updates();
        }
        if !update_idle(
            self.capture.is_some(),
            self.capturing,
            self.image_workers,
            self.panel.visible,
            self.quit,
        ) {
            return;
        }
        if self.updates.apply.is_some() {
            // The helper waits on its uncommitted pipe. A capture or open popup
            // that began during helper setup has now finished naturally.
            self.updates.exit_for_update = true;
            self.quit = true;
        } else if !self.updates.applying
            && let Some(prepared) = self.updates.prepared.as_ref().map(Arc::clone)
        {
            self.updates.applying = true;
            self.updates.status = UpdateStatus::Installing(prepared.version_string());
            self.updates.next_check = None;
            native::worker(self.sender.clone(), self.host.handle, move || {
                Reply::UpdateApplyReady(
                    updater::start_apply(&prepared).map_err(|error| error.to_string()),
                )
            });
        }
    }
    fn copied(&mut self, display: Rect) {
        let scale = native::display_scale(POINT {
            x: (display.origin.x + display.size.width * 0.5) as i32,
            y: (display.origin.y + display.size.height * 0.5) as i32,
        });
        if let Ok(window) = Window::new(
            "Copied to clipboard",
            r(
                display.origin.x + (display.size.width - 248. * scale) / 2.,
                display.origin.y + 40. * scale,
                248. * scale,
                48. * scale,
            ),
            true,
        ) {
            window.show(false);
            self.toast = Some((window, Instant::now() + Duration::from_millis(1200)));
            self.dirty = true;
        }
    }
    fn finish_selection(&mut self) {
        let Some(c) = self.capture.as_mut() else {
            return;
        };
        c.start = None;
        let bounds = r(0., 0., c.desktop.size.width, c.desktop.size.height);
        let crop = match image::crop(&c.original, c.selection, bounds) {
            Ok(image) => image,
            Err(_) => {
                c.selection = Rect::ZERO;
                self.dirty = true;
                return;
            }
        };
        c.crop = Some(crop.clone());
        c.redaction_selection = c.selection;
        if c.mode == CaptureMode::Copy {
            self.export();
            return;
        }
        c.raster = Some(crop.clone());
        let d = c.display;
        let s = c.scale;
        let target = geometry::fitted_canvas(
            Size::new(crop.width() as f64, crop.height() as f64),
            r(
                d.origin.x + 48. * s,
                d.origin.y + 48. * s,
                (d.size.width - 96. * s).max(1.),
                (d.size.height - 176. * s).max(1.),
            ),
        );
        if native::reduce_motion() {
            c.selection = target;
        } else {
            c.zoom = Some(Zoom {
                started: Instant::now(),
                from: c.selection,
                to: target,
            });
        }
        c.input = match text::Input::new(c.window.handle, s) {
            Ok(input) => Some(input),
            Err(error) => {
                c.message = Some(error);
                None
            }
        };
        c.brush_cursor = native::BrushCursor::new(s).ok();
        if c.zoom.is_none() {
            refresh_input(c, true);
        }
        self.dirty = true;
        self.warm_pixelation();
    }
    fn warm_pixelation(&mut self) {
        let Some(c) = self.capture.as_mut() else {
            return;
        };
        if c.busy {
            return;
        }
        let Some(crop) = c.crop.clone() else {
            return;
        };
        let canvas = c.zoom.as_ref().map_or(c.selection, |z| z.to);
        let cell =
            ((12. * c.scale * crop.width() as f64 / canvas.size.width).round() as usize).max(1);
        let Some((revision, seed, tone)) = c.editor.pixelation_job() else {
            return;
        };
        let width = crop.width();
        let height = crop.height();
        let source = c.original.clone();
        let selection = c.redaction_selection;
        let bounds = r(0., 0., c.desktop.size.width, c.desktop.size.height);
        let generation = self.generation;
        self.image_workers += 1;
        native::worker(self.sender.clone(), self.host.handle, move || {
            let automatic = if tone == RedactionTone::Auto {
                redaction_palette(&source, selection, bounds)
            } else {
                crate::redaction::Palette::DARK
            };
            Reply::Mosaic(
                generation,
                revision,
                crate::redaction::generate(width, height, cell, seed, tone.palette(automatic))
                    .and_then(Stamp::new),
            )
        });
    }
    fn export(&mut self) {
        if !self.commit_text() {
            return;
        }
        let Some(c) = self.capture.as_mut() else {
            return;
        };
        if c.busy {
            return;
        }
        let Some(image) = c.crop.clone() else {
            return;
        };
        c.editor.commit_current();
        c.busy = true;
        refresh_input(c, false);
        self.deadline = Some(Instant::now() + Duration::from_secs(30));
        c.message = None;
        let marks = c.editor.marks.clone();
        let generation = self.generation;
        self.image_workers += 1;
        native::worker(self.sender.clone(), self.host.handle, move || {
            Reply::Exported(
                generation,
                image::render(&image, &marks).and_then(|image| image::encode(&image)),
            )
        });
        self.dirty = true;
    }
    fn commit_text(&mut self) -> bool {
        let Some(c) = self.capture.as_mut() else {
            return true;
        };
        let Some(input) = &c.input else {
            return true;
        };
        let value = input.value();
        if value.is_empty() {
            return true;
        }
        if value.trim().is_empty() {
            input.clear();
            return true;
        }
        if c.busy {
            return false;
        }
        let Some(crop) = &c.crop else {
            return false;
        };
        let scale = c.scale * crop.width() as f64 / c.selection.size.width;
        let result = text::stamp(
            &value,
            COLORS[c.editor.color_index],
            scale,
            (crop.width() as f64 - c.text_origin.x).min(560. * scale),
            crop.height() as f64 - c.text_origin.y,
        );
        let accepted = match result {
            Ok(stamp) => c.editor.insert_text(c.text_origin, stamp),
            Err(error) => {
                c.message = Some(error);
                false
            }
        };
        if accepted {
            input.clear();
            refresh_input(c, true);
            self.schedule_raster();
        }
        if self
            .capture
            .as_ref()
            .is_some_and(|c| c.editor.tool.is_pixelation())
        {
            self.warm_pixelation();
        }
        self.dirty = true;
        accepted
    }
    fn schedule_raster(&mut self) {
        let Some(c) = self.capture.as_mut() else {
            return;
        };
        if c.busy {
            return;
        }
        let Some(image) = c.raster.clone() else {
            return;
        };
        let Some(request) = c.cache.request(c.editor.marks.len()) else {
            return;
        };
        let marks = c.editor.marks[request.from..request.to].to_vec();
        let generation = self.generation;
        self.image_workers += 1;
        native::worker(self.sender.clone(), self.host.handle, move || {
            Reply::Raster(generation, request, image::render(&image, &marks))
        });
    }
    fn history(&mut self, redo: bool) {
        if let Some(c) = self.capture.as_mut() {
            if c.busy {
                return;
            }
            if redo {
                c.editor.redo();
            } else {
                c.editor.undo();
            }
            if c.cache.truncate(c.editor.marks.len()) {
                c.raster = c.crop.clone();
            }
            self.dirty = true;
        }
        if self
            .capture
            .as_ref()
            .is_some_and(|c| c.editor.tool.is_pixelation())
        {
            self.warm_pixelation();
        }
        self.schedule_raster();
    }
    fn buttons(&self) -> &[Button] {
        self.capture
            .as_ref()
            .map_or(&self.host.buttons, |c| &c.window.buttons)
    }
    fn activate(&mut self, id: i32) {
        if (20..=42).contains(&id) && !self.commit_text() {
            return;
        }
        match id {
            1 => self.start(CaptureMode::Copy),
            2 => self.start(CaptureMode::Markup),
            3 => {
                if self.capture.is_none() {
                    self.start(CaptureMode::Copy);
                }
            }
            4 => self.show_menu(),
            5 => self.quit = true,
            6 => self.cancel(),
            10 => {
                let next = !self.login;
                match if self.ui_test {
                    Ok(())
                } else {
                    native::set_login(next)
                } {
                    Ok(()) => self.login = next,
                    Err(e) => self.error = Some(e),
                }
            }
            11 | 12 => {
                let next = id == 12;
                let result = if self.ui_test {
                    Ok(())
                } else {
                    native::save_modified(next).map_err(|e| e.to_string())
                };
                if let Err(e) = result {
                    self.error = Some(e);
                } else {
                    self.modified = next;
                    if !self.ui_test {
                        self.shortcut_error = self
                            .hotkeys
                            .as_mut()
                            .and_then(|keys| keys.register(next).err());
                    }
                }
            }
            15 => {
                if matches!(self.updates.status, UpdateStatus::Ready(_)) {
                    if self.capture.is_none() && !self.capturing {
                        self.dismiss_panel(true);
                    }
                } else if update_action_enabled(&self.updates.status) {
                    self.check_for_updates();
                }
            }
            20..=26 => {
                if let Some(c) = self.capture.as_mut()
                    && !c.busy
                {
                    c.editor.tool = Tool::ALL[(id - 20) as usize];
                }
            }
            30..=33 => {
                if let Some(c) = self.capture.as_mut()
                    && !c.busy
                {
                    c.editor.color_index = (id - 30) as usize;
                }
            }
            34..=36 => {
                if let Some(c) = self.capture.as_mut()
                    && !c.busy
                {
                    c.editor
                        .set_redaction_tone(RedactionTone::ALL[(id - 34) as usize]);
                }
            }
            40 => self.history(false),
            41 => self.history(true),
            42 => self.export(),
            _ => {}
        }
        if let Some(c) = self.capture.as_mut() {
            refresh_input(c, true);
        }
        if self
            .capture
            .as_ref()
            .is_some_and(|c| c.editor.tool.is_pixelation())
        {
            self.warm_pixelation();
        }
        self.dirty = true;
    }
    fn key(&mut self, key: u32, repeated: bool, control: bool, shift: bool, alt: bool) {
        if key == VK_ESCAPE.0 as u32 {
            if self.capture.is_some() || self.capturing {
                self.cancel();
            } else {
                self.dismiss_panel(true);
                self.panel.tray_press = None;
            }
            return;
        }
        if repeated
            && [
                VK_UP.0 as u32,
                VK_DOWN.0 as u32,
                VK_RETURN.0 as u32,
                VK_SPACE.0 as u32,
            ]
            .contains(&key)
        {
            return;
        }
        if key == VK_TAB.0 as u32 {
            let ids: Vec<_> = self
                .buttons()
                .iter()
                .filter(|b| b.enabled)
                .map(|b| b.id)
                .collect();
            if !ids.is_empty() {
                let current = self.focus.and_then(|id| ids.iter().position(|x| *x == id));
                let index = match (current, shift) {
                    (Some(i), false) => (i + 1) % ids.len(),
                    (Some(i), true) => (i + ids.len() - 1) % ids.len(),
                    (None, false) => 0,
                    (None, true) => ids.len() - 1,
                };
                self.focus = Some(ids[index]);
                if let Some(c) = &self.capture {
                    unsafe {
                        let _ = SetFocus(Some(c.window.handle));
                    }
                }
                self.dirty = true;
            }
            return;
        }
        if self.capture.is_none()
            && self.panel.visible
            && (key == VK_RETURN.0 as u32 || key == VK_SPACE.0 as u32)
        {
            self.activate(self.focus.unwrap_or(1));
            return;
        }
        if key == VK_SPACE.0 as u32
            && let Some(id) = self.focus.filter(|id| *id > 0)
        {
            self.activate(id);
            return;
        }
        let id = if key == VK_UP.0 as u32 { 1 } else { 2 };
        let registered = self
            .hotkeys
            .as_ref()
            .is_some_and(|keys| keys.registered(id));
        if let Some(id) = local_capture_action(
            key,
            repeated,
            control,
            shift,
            alt,
            self.modified,
            registered,
        ) && (self.capture.is_some() || self.panel.visible)
        {
            self.activate(id);
            return;
        }
        if self.capture.as_ref().is_some_and(|c| c.busy) {
            return;
        }
        if control && key == b'Z' as u32 {
            self.history(shift);
            return;
        }
        if key == VK_RETURN.0 as u32 || (control && key == b'C' as u32) {
            self.export();
            return;
        }
        if let Some(c) = self.capture.as_mut()
            && key == VK_SPACE.0 as u32
            && c.crop.is_none()
        {
            c.selection = c.display;
            self.finish_selection();
        }
    }
    fn pointer(&mut self, hwnd: HWND, point: Point, phase: u8) {
        if phase == 1 && !self.commit_text() {
            return;
        }
        let overlay = self
            .capture
            .as_ref()
            .is_some_and(|c| c.window.handle == hwnd);
        if !overlay && (hwnd != self.host.handle || !self.panel.visible) {
            return;
        }
        let scale = 1.;
        let p = Point::new(point.x / scale, point.y / scale);
        let hit = self
            .buttons()
            .iter()
            .find(|b| b.enabled && contains(b.rect, p))
            .map(|b| b.id);
        if phase == 0 {
            if self.hover != hit {
                self.hover = hit;
                self.dirty = true;
            }
            unsafe {
                let brush = self
                    .capture
                    .as_ref()
                    .filter(|c| {
                        c.editor.tool == Tool::PixelateBrush && c.crop.is_some() && hit.is_none()
                    })
                    .and_then(|c| c.brush_cursor.as_ref());
                if let Some(brush) = brush {
                    SetCursor(Some(brush.0));
                } else if let Ok(cursor) = LoadCursorW(
                    None,
                    if overlay && hit.is_none() {
                        if self
                            .capture
                            .as_ref()
                            .is_some_and(|c| c.editor.tool == Tool::Text)
                        {
                            IDC_IBEAM
                        } else {
                            IDC_CROSS
                        }
                    } else {
                        IDC_ARROW
                    },
                ) {
                    SetCursor(Some(cursor));
                }
            }
        }
        if phase == 1 {
            self.focus = None;
            self.pressed = hit;
            unsafe {
                SetCapture(hwnd);
            }
            if hit.is_some() {
                self.dirty = true;
                return;
            }
        }
        if phase == 2 {
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some(id) = self.pressed.take() {
                if hit == Some(id) {
                    self.activate(id);
                }
                return;
            }
        }
        if !overlay {
            return;
        }
        let Some(c) = self.capture.as_mut() else {
            return;
        };
        if c.busy || c.zoom.is_some() {
            return;
        }
        if let Some(crop) = c.crop.as_ref() {
            let pixel = geometry::image_point(
                point,
                c.selection,
                Size::new(crop.width() as f64, crop.height() as f64),
            );
            if phase == 1 && contains(c.selection, point) {
                c.message = None;
                c.text_origin = pixel;
                if c.editor.tool == Tool::Text {
                    refresh_input(c, true);
                    return;
                }
                if c.editor.tool.is_pixelation() && !c.editor.pixelation_ready() {
                    c.message = Some("Preparing redaction…".into());
                    self.warm_pixelation();
                    self.dirty = true;
                    return;
                }
                if !c.editor.begin(
                    pixel,
                    c.scale * crop.width() as f64 / c.selection.size.width,
                ) {
                    c.message = Some("Canvas is full. Copy it or undo a mark.".into());
                }
            }
            if phase == 0 && c.editor.current.is_some() {
                c.editor.drag(pixel);
                self.dirty = true;
            }
            if phase == 2 && c.editor.current.is_some() {
                c.editor.end(pixel);
                refresh_input(c, true);
                self.schedule_raster();
            }
        } else {
            if phase == 1 {
                c.message = None;
                c.start = Some(point);
                let global = POINT {
                    x: (point.x + c.desktop.origin.x) as i32,
                    y: (point.y + c.desktop.origin.y) as i32,
                };
                let (display, rate) = native::monitor(global);
                c.display = relative(display, c.desktop.origin);
                c.scale = native::display_scale(global).min(display.size.width / 880.);
                self.rate = rate;
            }
            if let Some(start) = c.start {
                c.selection = geometry::selection(
                    start,
                    point,
                    r(0., 0., c.desktop.size.width, c.desktop.size.height),
                );
                self.dirty = true;
                if phase == 2 {
                    self.finish_selection();
                }
            }
        }
        if phase != 0 {
            self.dirty = true;
        }
    }
    fn event(&mut self, event: Event) {
        match event {
            Event::Paint => self.dirty = true,
            Event::CaptureLost => {
                self.pressed = None;
                if let Some(c) = self.capture.as_mut() {
                    c.editor.current = None;
                    if c.start.take().is_some() {
                        c.selection = Rect::ZERO;
                    }
                }
                self.dirty = true;
            }
            Event::Size(hwnd, w, h) => {
                let window = if self.host.handle == hwnd {
                    Some(&mut self.host)
                } else {
                    self.capture
                        .as_mut()
                        .filter(|c| c.window.handle == hwnd)
                        .map(|c| &mut c.window)
                };
                if let Some(window) = window {
                    window.size = Size::new(w as f64, h as f64);
                    if let Some(renderer) = window.renderer.as_mut()
                        && renderer.resize(w, h).is_err()
                    {
                        window.renderer = None;
                    }
                    self.dirty = true;
                }
            }
            Event::Move(h, p) => self.pointer(h, p, 0),
            Event::Leave(hwnd) => {
                if hwnd == self.host.handle
                    || self
                        .capture
                        .as_ref()
                        .is_some_and(|c| c.window.handle == hwnd)
                {
                    self.hover = None;
                    self.dirty = true;
                }
            }
            Event::Down(h, p) => self.pointer(h, p, 1),
            Event::Up(h, p) => self.pointer(h, p, 2),
            Event::Key(key, repeat, control, shift) => self.key(key, repeat, control, shift, false),
            Event::AltKey(key, repeat, control, shift) => {
                self.key(key, repeat, control, shift, true)
            }
            Event::Character(ch) => {
                if ch >= 32
                    && let Some(c) = self.capture.as_mut()
                    && !c.busy
                    && let Some(input) = &c.input
                {
                    input.focus();
                    unsafe {
                        SendMessageW(
                            input.handle,
                            WM_CHAR,
                            Some(WPARAM(ch as usize)),
                            Some(LPARAM(0)),
                        );
                    }
                }
            }
            Event::TextChanged => {
                if let Some(c) = self.capture.as_mut() {
                    refresh_input(c, false);
                }
            }
            Event::TextFinish(copy) => {
                if self.commit_text() && copy {
                    self.export();
                }
            }
            Event::TextDiscard => {
                if let Some(c) = self.capture.as_mut() {
                    if let Some(input) = &c.input {
                        input.clear();
                    }
                    refresh_input(c, true);
                }
            }
            Event::Hotkey(id) => self.activate(id),
            Event::Close(hwnd) => {
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|c| c.window.handle == hwnd)
                {
                    self.cancel();
                } else if hwnd == self.host.handle {
                    self.dismiss_panel(true);
                    self.panel.tray_press = None;
                }
            }
            Event::Deactivate(hwnd, point, time) => {
                if hwnd == self.host.handle && self.panel.visible {
                    let on_tray = self
                        .tray
                        .as_ref()
                        .and_then(|tray| tray.icon_bounds())
                        .is_some_and(|bounds| contains(bounds, point));
                    self.panel.deactivate(on_tray, time);
                    // An outside click has already chosen its destination; do
                    // not steal focus back from the newly activated app.
                    self.dismiss_panel(false);
                }
            }
            Event::Dpi(hwnd) => {
                if hwnd == self.host.handle && self.panel.visible {
                    self.position_panel();
                }
                self.dirty = true;
            }
            Event::Show => self.show_menu(),
            Event::Display => {
                if self.capture.is_some() || self.capturing {
                    self.error("Display configuration changed. Start a new capture.".into());
                }
                self.rate = native::monitor(native::cursor()).1;
                if self.panel.visible {
                    self.position_panel();
                }
                self.dirty = true;
            }
            Event::RestoreTray => {
                if let Some(tray) = &self.tray {
                    let _ = tray.restore();
                }
                if self.panel.visible {
                    self.position_panel();
                }
            }
            Event::Tray(msg, time) => {
                if msg == WM_LBUTTONDOWN {
                    self.panel.tray_down(time);
                    if self.panel.visible {
                        self.dismiss_panel(false);
                    }
                } else if msg == WM_LBUTTONUP {
                    if self.panel.tray_release(time) {
                        self.show_menu();
                    } else {
                        self.dismiss_panel(false);
                    }
                } else if msg == WM_RBUTTONUP || msg == WM_CONTEXTMENU {
                    self.fallback_menu();
                }
            }
        }
    }
    fn draw(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, expiry)| now >= *expiry)
        {
            self.toast = None;
        }
        if let Some(c) = self.capture.as_mut() {
            if let Some(z) = &c.zoom {
                let t = (now - z.started).as_secs_f64() / refresh::ZOOM_SECONDS;
                c.selection = interpolate(z.from, z.to, refresh::ease_out(t));
                if t >= 1. {
                    c.zoom = None;
                    refresh_input(c, true);
                }
            }
            draw_capture(c, self.hover, self.focus)?;
        }
        if self.panel.visible {
            self.draw_panel()?;
        }
        if let Some((toast, _)) = self.toast.as_mut() {
            let scale = toast.size.height / 48.;
            let renderer = toast.renderer()?;
            renderer.begin(render::SURFACE);
            renderer.scale(scale);
            renderer.text("✓", r(16., 10., 24., 28.), 18, true, render::LIME);
            renderer.text(
                "Copied to clipboard",
                r(48., 14., 184., 28.),
                14,
                true,
                render::TEXT,
            );
            renderer.end().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn draw_panel(&mut self) -> Result<(), String> {
        let buttons = panel_buttons(self.modified, self.login, 1., &self.updates.status);
        let error = self.error.as_deref().or(self.shortcut_error.as_deref());
        let renderer = self.host.renderer()?;
        renderer.begin(render::BG);
        renderer.scale(self.panel_scale);
        renderer.outline(
            r(
                0.5,
                0.5,
                native::PANEL_WIDTH - 1.,
                native::PANEL_HEIGHT - 1.,
            ),
            12.,
            0x383a3d,
            1.,
        );
        draw_brand(renderer, Point::new(20., 20.), 36.);
        renderer.text("Still", r(68., 17., 248., 30.), 22, true, render::TEXT);
        renderer.fill(
            r(69., 53., 5., 5.),
            2.5,
            if error.is_some() {
                render::ERROR
            } else {
                render::LIME
            },
        );
        let status = panel_status(error);
        renderer.text(
            &status,
            r(81., 47., 237., 29.),
            if error.is_some() { 11 } else { 12 },
            false,
            if error.is_some() {
                render::ERROR
            } else {
                render::SECONDARY
            },
        );
        for b in &buttons[..2] {
            button(renderer, b, self.hover, self.focus);
        }
        renderer.line(
            Point::new(20., 264.),
            Point::new(320., 264.),
            render::RAISED,
            1.,
        );
        let login = &buttons[2];
        panel_row(renderer, login, self.hover, self.focus);
        renderer.text(
            "Launch at login",
            r(24., 280., 232., 24.),
            13,
            false,
            render::TEXT,
        );
        renderer.fill(
            r(278., 281., 32., 18.),
            9.,
            if self.login {
                render::LIME
            } else {
                render::RAISED
            },
        );
        renderer.fill(
            r(if self.login { 293. } else { 281. }, 284., 12., 12.),
            6.,
            if self.login {
                render::INK
            } else {
                render::SECONDARY
            },
        );
        renderer.text(
            "Shortcuts",
            r(24., 320., 80., 22.),
            12,
            false,
            render::SECONDARY,
        );
        for b in &buttons[3..5] {
            panel_choice(renderer, b, self.hover, self.focus);
        }
        renderer.line(
            Point::new(20., 360.),
            Point::new(320., 360.),
            render::RAISED,
            1.,
        );
        let update = &buttons[5];
        panel_row(renderer, update, self.hover, self.focus);
        renderer.text(
            &self.updates.status.label(),
            r(24., 375., 210., 22.),
            11,
            false,
            match self.updates.status {
                UpdateStatus::Ready(_) => render::LIME,
                UpdateStatus::Failed(_) => render::ERROR,
                UpdateStatus::NotConfigured => render::MUTED,
                _ => render::SECONDARY,
            },
        );
        let quit = &buttons[6];
        panel_row(renderer, quit, self.hover, self.focus);
        renderer.text(
            "Quit Still",
            r(260., 375., 64., 22.),
            12,
            false,
            render::SECONDARY,
        );
        renderer.end().map_err(|e| e.to_string())?;
        self.host.buttons = panel_buttons(
            self.modified,
            self.login,
            self.panel_scale,
            &self.updates.status,
        );
        Ok(())
    }
}
fn panel_status(error: Option<&str>) -> String {
    let Some(error) = error else {
        return "Ready to capture".into();
    };
    let mut text: String = error.chars().take(76).collect();
    if error.chars().count() > 76 {
        text.push('…');
    }
    text
}
fn panel_buttons(modified: bool, login: bool, scale: f64, status: &UpdateStatus) -> Vec<Button> {
    let mut copy = Button::new(1, r(16., 80., 308., 80.), "Capture & copy");
    copy.subtitle = "Select a region, then copy".into();
    copy.key = if modified { "Ctrl+Alt ↑" } else { "↑" }.into();
    copy.primary = true;
    let mut draw = Button::new(2, r(16., 168., 308., 80.), "Capture & draw");
    draw.subtitle = "Select, annotate, and copy".into();
    draw.key = if modified { "Ctrl+Alt ↓" } else { "↓" }.into();
    let mut launch = Button::new(10, r(16., 272., 308., 36.), "Launch at login");
    launch.selected = login;
    let mut plain = Button::new(11, r(110., 312., 64., 32.), "↑ / ↓");
    plain.selected = !modified;
    let mut alternate = Button::new(12, r(182., 312., 142., 32.), "Ctrl+Alt ↑ / ↓");
    alternate.selected = modified;
    let mut update = Button::new(15, r(16., 368., 226., 28.), &status.label());
    update.enabled = update_action_enabled(status);
    let quit = Button::new(5, r(250., 368., 74., 28.), "Quit Still");
    let mut buttons = vec![copy, draw, launch, plain, alternate, update, quit];
    for b in &mut buttons {
        b.rect = r(
            b.rect.origin.x * scale,
            b.rect.origin.y * scale,
            b.rect.size.width * scale,
            b.rect.size.height * scale,
        );
    }
    buttons
}
fn draw_brand(renderer: &Renderer, origin: Point, size: f64) {
    renderer.fill(r(origin.x, origin.y, size, size), 9., render::LIME);
    let inset = size * 0.25;
    let arm = size * 0.17;
    for (right, bottom) in [(false, false), (true, false), (false, true), (true, true)] {
        let x = origin.x + if right { size - inset } else { inset };
        let y = origin.y + if bottom { size - inset } else { inset };
        renderer.line(
            Point::new(x, y),
            Point::new(x + if right { -arm } else { arm }, y),
            render::INK,
            1.8,
        );
        renderer.line(
            Point::new(x, y),
            Point::new(x, y + if bottom { -arm } else { arm }),
            render::INK,
            1.8,
        );
    }
    for fraction in [0.46, 0.56] {
        renderer.line(
            Point::new(origin.x + size * fraction, origin.y + size * 0.43),
            Point::new(origin.x + size * fraction, origin.y + size * 0.57),
            render::INK,
            1.8,
        );
    }
}
fn panel_row(renderer: &Renderer, b: &Button, hover: Option<i32>, focus: Option<i32>) {
    if b.enabled && hover == Some(b.id) {
        renderer.fill(b.rect, 6., render::SURFACE);
    }
    if b.enabled && focus == Some(b.id) {
        renderer.outline(b.rect, 6., render::LIME, 1.5);
    }
}
fn panel_choice(renderer: &mut Renderer, b: &Button, hover: Option<i32>, focus: Option<i32>) {
    renderer.fill(
        b.rect,
        6.,
        if b.selected || hover == Some(b.id) {
            render::RAISED
        } else {
            render::SURFACE
        },
    );
    if b.selected || focus == Some(b.id) {
        renderer.outline(
            b.rect,
            6.,
            if focus == Some(b.id) {
                render::LIME
            } else {
                0x565b50
            },
            if focus == Some(b.id) { 1.5 } else { 1. },
        );
    }
    renderer.text(
        &b.label,
        r(
            b.rect.origin.x + 10.,
            b.rect.origin.y + 7.,
            b.rect.size.width - 20.,
            22.,
        ),
        12,
        b.selected,
        if b.selected {
            render::LIME
        } else {
            render::SECONDARY
        },
    );
}
fn refresh_input(c: &mut Capture, focus: bool) {
    let Some(input) = &c.input else {
        return;
    };
    let Some(crop) = &c.crop else {
        return;
    };
    if c.zoom.is_some() {
        return;
    }
    let ratio = c.selection.size.width / crop.width() as f64;
    c.text_origin.x = c
        .text_origin
        .x
        .max(0.)
        .min((crop.width() as f64 - 96. * c.scale / ratio).max(0.));
    c.text_origin.y = c
        .text_origin
        .y
        .max(0.)
        .min((crop.height() as f64 - 64. * c.scale / ratio).max(0.));
    let x = c.selection.origin.x + c.text_origin.x * ratio;
    let y = c.selection.origin.y + c.text_origin.y * ratio;
    let visible = !input.value().is_empty() || c.editor.tool == Tool::Text;
    let color = COLORS[c.editor.color_index];
    text::INK.with(|ink| {
        ink.set(
            (color[0] * 255.) as u32
                | ((color[1] * 255.) as u32) << 8
                | ((color[2] * 255.) as u32) << 16,
        )
    });
    let size = input.content_size(
        (c.selection.origin.x + c.selection.size.width - x).clamp(1., 560. * c.scale),
        (c.selection.origin.y + c.selection.size.height - y).clamp(1., 240. * c.scale),
        c.scale,
    );
    input.position(
        r(x, y, size.width, size.height),
        visible && !c.busy,
        c.scale,
    );
    unsafe {
        let _ = EnableWindow(input.handle, !c.busy);
    }
    if focus && !c.busy {
        input.focus();
    }
}
fn button(renderer: &mut Renderer, b: &Button, hover: Option<i32>, focus: Option<i32>) {
    let ink = if !b.enabled {
        render::MUTED
    } else if b.primary {
        render::INK
    } else {
        render::TEXT
    };
    renderer.fill(
        b.rect,
        8.,
        if b.primary && b.enabled {
            render::LIME
        } else if b.selected || hover == Some(b.id) {
            render::RAISED
        } else {
            render::SURFACE
        },
    );
    if b.selected {
        renderer.outline(b.rect, 8., 0x46494d, 1.);
    }
    if focus == Some(b.id) {
        renderer.outline(
            r(
                b.rect.origin.x - 2.,
                b.rect.origin.y - 2.,
                b.rect.size.width + 4.,
                b.rect.size.height + 4.,
            ),
            10.,
            render::LIME,
            2.,
        );
    }
    if let Some(color) = b.swatch {
        renderer.fill(
            r(
                b.rect.origin.x + (b.rect.size.width - 16.) / 2.,
                b.rect.origin.y + (b.rect.size.height - 16.) / 2.,
                16.,
                16.,
            ),
            8.,
            color,
        );
        return;
    }
    let compact = b.rect.size.width <= 80. && b.key.is_empty();
    let redaction_tool = matches!(b.id, 25 | 26);
    let action = !b.subtitle.is_empty();
    let short_action = action && b.rect.size.height <= 80.;
    let padding = if action {
        if short_action { 16. } else { 20. }
    } else if compact {
        6.
    } else {
        12.
    };
    let key_width = if b.key.is_empty() {
        0.
    } else {
        (b.key.chars().count() as f64 * 7. + 16.).max(28.)
    };
    let text_width =
        b.rect.size.width - padding * 2. - if key_width > 0. { key_width + 8. } else { 0. };
    renderer.text(
        &b.label,
        r(
            b.rect.origin.x + padding,
            b.rect.origin.y
                + if redaction_tool {
                    5.
                } else if action {
                    if short_action { 14. } else { 20. }
                } else {
                    (b.rect.size.height - 20.) / 2.
                },
            text_width,
            if action || redaction_tool { 32. } else { 24. },
        ),
        if action {
            if short_action { 16 } else { 20 }
        } else if redaction_tool {
            11
        } else if compact {
            12
        } else {
            14
        },
        true,
        ink,
    );
    if action {
        renderer.text(
            &b.subtitle,
            r(
                b.rect.origin.x + padding,
                b.rect.origin.y + if short_action { 44. } else { 58. },
                text_width,
                24.,
            ),
            if short_action { 12 } else { 13 },
            false,
            if b.primary {
                render::INK
            } else {
                render::SECONDARY
            },
        );
    }
    if !b.key.is_empty() {
        let w = key_width;
        let rect = r(
            b.rect.origin.x + b.rect.size.width - w - 12.,
            b.rect.origin.y + (b.rect.size.height - 28.) / 2.,
            w,
            28.,
        );
        if b.primary {
            renderer.fill_alpha(rect, 6., render::INK, 0.08);
        } else {
            renderer.fill(rect, 6., render::RAISED);
        }
        renderer.text(
            &b.key,
            r(rect.origin.x + 8., rect.origin.y + 4., w - 8., 24.),
            13,
            true,
            ink,
        );
    }
}
fn draw_capture(c: &mut Capture, hover: Option<i32>, focus: Option<i32>) -> Result<(), String> {
    let editing = c.crop.is_some() && c.raster.is_some();
    let display = c.display;
    let scale = c.scale;
    let size = c.desktop.size;
    let renderer = c.window.renderer()?;
    renderer.begin(render::DEEP);
    if editing {
        let crop = c.crop.as_ref().unwrap();
        renderer
            .image(c.raster.as_ref().unwrap(), c.selection)
            .map_err(|e| e.to_string())?;
        for mark in &c.editor.marks[c.cache.cached_count()..] {
            renderer
                .mark(mark, c.selection, crop)
                .map_err(|e| e.to_string())?;
        }
        if let Some(mark) = &c.editor.current {
            renderer
                .mark(mark, c.selection, crop)
                .map_err(|e| e.to_string())?;
        }
        renderer.outline(c.selection, 0., 0x45474a, 1.);
    } else {
        renderer
            .image(&c.original, r(0., 0., size.width, size.height))
            .map_err(|e| e.to_string())?;
        let sel = c.selection;
        let right = sel.origin.x + sel.size.width;
        let bottom = sel.origin.y + sel.size.height;
        if sel.size.width > 1. && sel.size.height > 1. {
            for area in [
                r(0., 0., size.width, sel.origin.y),
                r(0., bottom, size.width, size.height - bottom),
                r(0., sel.origin.y, sel.origin.x, sel.size.height),
                r(right, sel.origin.y, size.width - right, sel.size.height),
            ] {
                renderer.fill_alpha(area, 0., 0, 0.48);
            }
            renderer.outline(sel, 0., render::LIME, 1.5);
            renderer.text(
                &format!(
                    "{} × {}",
                    sel.size.width.ceil() as u32,
                    sel.size.height.ceil() as u32
                ),
                r(sel.origin.x, (sel.origin.y - 28.).max(0.), 240., 24.),
                12,
                true,
                render::TEXT,
            );
        } else {
            renderer.fill_alpha(r(0., 0., size.width, size.height), 0., 0, 0.48);
        }
    }
    // Image coordinates remain physical pixels. Controls use monitor-scaled
    // logical coordinates; hit regions are converted back after drawing.
    renderer.scale(scale);
    let display = r(
        display.origin.x / scale,
        display.origin.y / scale,
        display.size.width / scale,
        display.size.height / scale,
    );
    let scale = 1.;
    let hud = r(
        display.origin.x + (display.size.width - 400. * scale) / 2.,
        display.origin.y + 24. * scale,
        400. * scale,
        48. * scale,
    );
    if !editing {
        renderer.fill(hud, 12., render::SURFACE);
    }
    let mut buttons = Vec::new();
    let mut cancel = Button::new(
        6,
        r(
            hud.origin.x + 352. * scale,
            hud.origin.y + 8. * scale,
            32. * scale,
            32. * scale,
        ),
        "×",
    );
    cancel.enabled = true;
    if editing {
        let bar = r(
            display.origin.x + (display.size.width - 848. * scale) / 2.,
            display.origin.y + display.size.height - 80. * scale,
            848. * scale,
            56. * scale,
        );
        renderer.fill(bar, 12., render::SURFACE);
        for (i, label) in [
            "Pen",
            "Arrow",
            "Box",
            "Mark",
            "Text",
            "Redact\narea",
            "Redact\nbrush",
        ]
        .into_iter()
        .enumerate()
        {
            let mut b = Button::new(
                20 + i as i32,
                r(
                    bar.origin.x + (8. + i as f64 * 56.) * scale,
                    bar.origin.y + 8. * scale,
                    52. * scale,
                    40. * scale,
                ),
                label,
            );
            b.selected = c.editor.tool == Tool::ALL[i];
            buttons.push(b);
        }
        if c.editor.tool.is_pixelation() {
            for (i, tone) in RedactionTone::ALL.into_iter().enumerate() {
                let mut b = Button::new(
                    34 + i as i32,
                    r(
                        bar.origin.x + (400. + i as f64 * 48.) * scale,
                        bar.origin.y + 8. * scale,
                        46. * scale,
                        40. * scale,
                    ),
                    tone.label(),
                );
                b.selected = c.editor.redaction_tone == tone;
                buttons.push(b);
            }
        } else {
            for (i, color) in COLORS.into_iter().enumerate() {
                let mut b = Button::new(
                    30 + i as i32,
                    r(
                        bar.origin.x + (408. + i as f64 * 32.) * scale,
                        bar.origin.y + 8. * scale,
                        28. * scale,
                        40. * scale,
                    ),
                    "",
                );
                b.swatch = Some(
                    ((color[0] * 255.).round() as u32) << 16
                        | ((color[1] * 255.).round() as u32) << 8
                        | (color[2] * 255.).round() as u32,
                );
                b.selected = c.editor.color_index == i;
                buttons.push(b);
            }
        }
        let mut undo = Button::new(
            40,
            r(
                bar.origin.x + 548. * scale,
                bar.origin.y + 8. * scale,
                64. * scale,
                40. * scale,
            ),
            "Undo",
        );
        undo.enabled = !c.editor.marks.is_empty() || c.editor.current.is_some();
        let mut redo = Button::new(
            41,
            r(
                bar.origin.x + 616. * scale,
                bar.origin.y + 8. * scale,
                64. * scale,
                40. * scale,
            ),
            "Redo",
        );
        redo.enabled = !c.editor.redo.is_empty();
        let mut copy = Button::new(
            42,
            r(
                bar.origin.x + 696. * scale,
                bar.origin.y + 8. * scale,
                96. * scale,
                40. * scale,
            ),
            "Copy",
        );
        copy.primary = true;
        copy.key = "↓".into();
        cancel.rect = r(
            bar.origin.x + 808. * scale,
            bar.origin.y + 8. * scale,
            32. * scale,
            40. * scale,
        );
        buttons.extend([undo, redo, copy]);
        let crop = c.crop.as_ref().unwrap();
        renderer.text(
            &format!(
                "{} × {} px · Type or paste to add text",
                crop.width(),
                crop.height()
            ),
            r(bar.origin.x, bar.origin.y - 36., bar.size.width, 28.),
            12,
            false,
            render::MUTED,
        );
    } else {
        renderer.logo(
            hud.origin.x + 16. * scale,
            hud.origin.y + 12. * scale,
            24. * scale,
        );
        let mut copy = Button::new(
            1,
            r(
                hud.origin.x + 52. * scale,
                hud.origin.y + 8. * scale,
                124. * scale,
                32. * scale,
            ),
            "Copy",
        );
        copy.key = "↑".into();
        copy.selected = c.mode == CaptureMode::Copy;
        let mut draw = Button::new(
            2,
            r(
                hud.origin.x + 180. * scale,
                hud.origin.y + 8. * scale,
                124. * scale,
                32. * scale,
            ),
            "Draw",
        );
        draw.key = "↓".into();
        draw.selected = c.mode == CaptureMode::Markup;
        buttons.extend([copy, draw]);
        if c.start.is_none() {
            let hint = r(
                display.origin.x + (display.size.width - 432. * scale) / 2.,
                display.origin.y + display.size.height - 64. * scale,
                432. * scale,
                40. * scale,
            );
            renderer.fill(hint, 12., render::SURFACE);
            renderer.text(
                "Drag to select    Space  Full screen    Esc  Cancel",
                r(
                    hint.origin.x + 16.,
                    hint.origin.y + 10.,
                    hint.size.width - 32.,
                    28.,
                ),
                12,
                false,
                render::TEXT,
            );
        }
    }
    for b in &mut buttons {
        b.enabled &= !c.busy;
    }
    buttons.push(cancel);
    for b in &buttons {
        button(renderer, b, hover, focus);
    }
    if let Some(error) = &c.message {
        renderer.fill(
            r(
                display.origin.x + 24.,
                display.origin.y + 88.,
                display.size.width - 48.,
                56.,
            ),
            8.,
            render::SURFACE,
        );
        renderer.text(
            error,
            r(
                display.origin.x + 40.,
                display.origin.y + 100.,
                display.size.width - 80.,
                44.,
            ),
            13,
            false,
            render::ERROR,
        );
    }
    renderer.end().map_err(|e| e.to_string())?;
    for b in &mut buttons {
        b.rect = r(
            b.rect.origin.x * c.scale,
            b.rect.origin.y * c.scale,
            b.rect.size.width * c.scale,
            b.rect.size.height * c.scale,
        );
    }
    c.window.buttons = buttons;
    Ok(())
}
fn execute() -> Result<(), String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| e.to_string())?;
        struct Com;
        impl Drop for Com {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _com = Com;
        let instance = CreateMutexW(None, false, w!("Local\\Still.Screenshot.SingleInstance"))
            .map_err(|e| e.to_string())?;
        struct Guard(HANDLE);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(instance);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(window) = FindWindowW(CLASS, w!("Still")) {
                let _ = PostMessageW(Some(window), native::SHOW, WPARAM(0), LPARAM(0));
            }
            return Ok(());
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(None).map_err(|e| e.to_string())?.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(|e| e.to_string())?,
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread().to_string());
        }
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        let mut app = App::new()?;
        let timer = native::Timer::new().map_err(|e| e.to_string())?;
        if std::env::args().any(|arg| arg == "--demo") {
            app.start(CaptureMode::Markup);
        }
        let mut failures = 0;
        while !app.quit {
            let mut message = MSG::default();
            for _ in 0..512 {
                if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                if message.message == WM_QUIT {
                    app.quit = true;
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            loop {
                let event = EVENTS.with(|events| events.borrow_mut().pop_front());
                if let Some(event) = event {
                    app.event(event);
                } else {
                    break;
                }
            }
            app.replies();
            let now = Instant::now();
            if app.deadline.is_some_and(|deadline| now >= deadline) {
                app.error(
                    "The capture took too long. Try a smaller region or capture again.".into(),
                );
            }
            if app.toast.as_ref().is_some_and(|(_, expiry)| now >= *expiry) {
                app.toast = None;
            }
            app.poll_updates(now);
            if app.quit {
                break;
            }
            let moving = app.capture.as_ref().is_some_and(|c| c.zoom.is_some());
            if (app.dirty || moving) && now >= app.next_frame {
                app.next_frame = refresh::next_frame_deadline(app.next_frame, now, app.rate);
                app.dirty = false;
                if let Err(error) = app.draw() {
                    failures += 1;
                    if failures > 2 {
                        return Err(format!("Windows could not render Still: {error}"));
                    }
                    if let Some(c) = app.capture.as_mut() {
                        c.window.renderer = None;
                        c.message = Some(
                            "The display renderer restarted. Your capture is preserved.".into(),
                        );
                    } else {
                        app.error = Some(format!("The graphics device changed. Retrying. {error}"));
                    }
                    app.host.renderer = None;
                    app.toast = None;
                    app.dirty = true;
                } else {
                    failures = 0;
                }
            }
            let moving = app.capture.as_ref().is_some_and(|c| c.zoom.is_some());
            let frame = (app.dirty || moving).then_some(app.next_frame);
            let expiry = [
                app.toast.as_ref().map(|(_, expiry)| *expiry),
                app.deadline,
                app.updates.next_check,
            ]
            .into_iter()
            .flatten()
            .min();
            let deadline = match (frame, expiry) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if !app.quit {
                timer.wait(deadline);
            }
        }
        if app.updates.exit_for_update
            && let Some(guard) = app.updates.apply.take()
        {
            // Commit only on the successful normal exit path. The helper cannot
            // install until the OS closes this process's retained pipe at exit.
            guard.commit_on_exit();
        }
        Ok(())
    }
}
fn self_test() -> Result<(), String> {
    panel_geometry_test()?;
    redaction_palette_test()?;
    update_policy_test()?;
    let source = image::synthetic(64, 48)?;
    let crop = image::crop(&source, r(5., 7., 21., 17.), r(0., 0., 64., 48.))?;
    let mut editor = Editor::default();
    editor.begin(Point::new(3., 3.), 1.);
    editor.end(Point::new(15., 12.));
    let encoded = image::encode(&image::render(&crop, &editor.marks)?)?;
    if encoded.png.len() < 32 || encoded.dibv5.len() != 124 + 21 * 17 * 4 {
        return Err("Image self-test failed".into());
    }
    if refresh::target_hz(240.) != 240. || refresh::target_hz(120.) != 120. {
        return Err("Refresh policy self-test failed".into());
    }
    native::validate_clipboard_buffers(&encoded).map_err(|e| e.to_string())?;
    native::Timer::new().map_err(|e| e.to_string())?;
    // Exercise actual Win32/Direct2D/DirectWrite marshalling against a hidden
    // window without tray icons, hotkeys, registry writes, or clipboard changes.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| e.to_string())?;
        struct Com;
        impl Drop for Com {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _com = Com;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(None).map_err(|e| e.to_string())?.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread().to_string());
        }
        panel_native_test()?;
        let mut window = Window::new("Still diagnostics", r(0., 0., 160., 120.), true)?;
        let input = text::Input::new(window.handle, 1.)?;
        let sample = "Typed or dictated — café, 日本語\r\nP A R H 123 stay text";
        let value = native::wide(sample);
        SetWindowTextW(input.handle, PCWSTR(value.as_ptr())).map_err(|e| e.to_string())?;
        if input.value() != sample {
            return Err("Native Unicode text insertion failed".into());
        }
        for contents in [w!("draft"), w!("")] {
            SetWindowTextW(input.handle, contents).map_err(|e| e.to_string())?;
            EVENTS.with(|events| events.borrow_mut().clear());
            for key in [VK_RETURN, VK_ESCAPE] {
                SendMessageW(
                    input.handle,
                    WM_KEYDOWN,
                    Some(WPARAM(key.0 as usize)),
                    Some(LPARAM(1 << 30)),
                );
            }
            if EVENTS.with(|events| !events.borrow().is_empty()) {
                return Err("Repeated text keys emitted a capture command".into());
            }
        }
        for key in [VK_UP, VK_DOWN] {
            for repeat in [false, true] {
                EVENTS.with(|events| events.borrow_mut().clear());
                let control = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                SendMessageW(
                    input.handle,
                    WM_KEYDOWN,
                    Some(WPARAM(key.0 as usize)),
                    Some(LPARAM(if repeat { 1 << 30 } else { 0 })),
                );
                let forwarded = EVENTS.with(|events| {
                    let mut events = events.borrow_mut();
                    events.len()==1 && matches!(events.pop_front(), Some(Event::Key(code, repeated, ctrl, shifted))
                        if code==key.0 as u32 && repeated==repeat && ctrl==control && shifted==shift)
                });
                if !forwarded {
                    return Err("Native text consumed an arrow capture command".into());
                }
            }
        }
        SetWindowTextW(input.handle, PCWSTR(value.as_ptr())).map_err(|e| e.to_string())?;
        let _cursor = native::BrushCursor::new(1.).map_err(|e| e.to_string())?;
        let fixture = image::synthetic(640, 480)?;
        if !editor.insert_text(
            Point::new(8., 8.),
            text::stamp(&input.value(), COLORS[0], 1., 600., 400.)?,
        ) {
            return Err("Text annotation insertion failed".into());
        }
        let (revision, seed, tone) = editor.pixelation_job().ok_or("Missing redaction job")?;
        let texture = Stamp::new(crate::redaction::generate(
            fixture.width(),
            fixture.height(),
            16,
            seed,
            tone.palette(crate::redaction::Palette::DARK),
        )?)?;
        editor.complete_pixelation(revision, Some(texture));
        for tool in [Tool::PixelateArea, Tool::PixelateBrush] {
            editor.tool = tool;
            editor.begin(Point::new(20., 20.), 1.);
            editor.end(Point::new(120., 100.));
        }
        if editor.marks.len() != 4
            || image::encode(&image::render(&fixture, &editor.marks)?)?
                .png
                .is_empty()
        {
            return Err("Annotation export self-test failed".into());
        }
        let renderer = window.renderer()?;
        renderer.begin(render::BG);
        renderer
            .image(&crop, r(8., 8., 84., 68.))
            .map_err(|e| e.to_string())?;
        for mark in &editor.marks {
            renderer
                .mark(mark, r(8., 8., 84., 68.), &fixture)
                .map_err(|e| e.to_string())?;
        }
        renderer.text("Still", r(8., 80., 144., 24.), 14, true, render::TEXT);
        renderer.end().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn redaction_palette_test() -> Result<(), String> {
    let bounds = r(7., 11., 40., 30.);
    let selection = r(12.125, 15.375, 18.2, 9.4);
    let exported = geometry::pixel_rect(selection, bounds, Size::new(80., 60.))
        .ok_or("Missing fractional crop in redaction self-test")?;
    let make = |hidden: [u8; 4]| {
        let mut rgba = [214, 211, 205, 255].repeat(80 * 60);
        for y in exported.origin.y as usize..(exported.origin.y + exported.size.height) as usize {
            for x in exported.origin.x as usize..(exported.origin.x + exported.size.width) as usize
            {
                rgba[(y * 80 + x) * 4..(y * 80 + x) * 4 + 4].copy_from_slice(&hidden);
            }
        }
        Image::from_rgba(80, 60, rgba)
    };
    let a = make([255, 255, 255, 255])?;
    let b = make([0, 0, 0, 255])?;
    let palette = redaction_palette(&a, selection, bounds);
    let other_palette = redaction_palette(&b, selection, bounds);
    if palette != other_palette
        || palette != crate::redaction::palette_from_samples(&[[214, 211, 205, 255]])
        || redaction_palette(&a, bounds, bounds) != crate::redaction::Palette::DARK
    {
        return Err("Automatic redaction tone sampled inside the exported crop".into());
    }
    let generate = |palette| {
        crate::redaction::generate(
            exported.size.width as usize,
            exported.size.height as usize,
            8,
            [19; 32],
            palette,
        )
    };
    if generate(palette)?.pixels() != generate(other_palette)?.pixels() {
        return Err("Redaction texture depends on hidden crop content".into());
    }
    Ok(())
}

fn panel_geometry_test() -> Result<(), String> {
    // Realistic top/bottom/side taskbars, negative desktop coordinates, mixed
    // DPI, an overflow/cursor anchor, and a work area smaller than the panel.
    for (anchor, work, dpi, edge) in [
        (r(1856., 1048., 24., 24.), r(0., 0., 1920., 1040.), 1., 0),
        (r(1856., 8., 24., 24.), r(0., 40., 1920., 1040.), 1., 1),
        (
            r(-1912., 800., 24., 24.),
            r(-1880., 0., 1880., 1080.),
            1.5,
            2,
        ),
        (r(1890., 800., 24., 24.), r(0., 0., 1880., 1080.), 1.5, 3),
        (
            r(-144., -48., 32., 32.),
            r(-2560., -1440., 2560., 1376.),
            2.,
            0,
        ),
        (r(1500., 800., 24., 24.), r(0., 0., 1920., 1040.), 1.25, 4),
        (r(-1200., 12., 1., 1.), r(-1280., 0., 1280., 680.), 2., 4),
    ] {
        let (bounds, scale) = native::panel_bounds(anchor, work, dpi);
        if bounds.origin.x < work.origin.x
            || bounds.origin.y < work.origin.y
            || bounds.origin.x + bounds.size.width > work.origin.x + work.size.width
            || bounds.origin.y + bounds.size.height > work.origin.y + work.size.height
            || scale <= 0.
            || scale > dpi
            || bounds.size.width != (native::PANEL_WIDTH * scale).round()
            || bounds.size.height != (native::PANEL_HEIGHT * scale).round()
        {
            return Err("Tray panel escaped its monitor work area or DPI scale".into());
        }
        let detached = match edge {
            0 => bounds.origin.y + bounds.size.height > anchor.origin.y,
            1 => bounds.origin.y < anchor.origin.y + anchor.size.height,
            2 => bounds.origin.x < anchor.origin.x + anchor.size.width,
            3 => bounds.origin.x + bounds.size.width > anchor.origin.x,
            _ => false,
        };
        if detached {
            return Err("Tray panel opened across its taskbar anchor".into());
        }
        for modified in [false, true] {
            let buttons = panel_buttons(modified, true, scale, &UpdateStatus::Idle);
            for b in &buttons {
                let center = Point::new(
                    b.rect.origin.x + b.rect.size.width / 2.,
                    b.rect.origin.y + b.rect.size.height / 2.,
                );
                if b.rect.origin.x < 0.
                    || b.rect.origin.y < 0.
                    || b.rect.origin.x + b.rect.size.width > bounds.size.width
                    || b.rect.origin.y + b.rect.size.height > bounds.size.height
                    || buttons
                        .iter()
                        .filter(|candidate| contains(candidate.rect, center))
                        .count()
                        != 1
                {
                    return Err("Tray panel controls overlap or escape their scaled bounds".into());
                }
            }
        }
    }
    let mut panel = PanelState::default();
    panel.tray_down(10);
    if !panel.tray_release(20) {
        return Err("First tray click did not open the panel".into());
    }
    panel.visible = true;
    panel.deactivate(true, 100);
    panel.tray_down(101);
    if panel.tray_release(200) {
        return Err("Deactivation reopened the panel on the same tray click".into());
    }
    panel.visible = true;
    panel.tray_down(300);
    panel.deactivate(false, 301);
    if panel.tray_release(9000) {
        return Err("A held tray click reopened the panel".into());
    }
    panel.visible = true;
    panel.deactivate(true, u32::MAX - 20);
    panel.tray_down(10);
    if panel.tray_release(30) {
        return Err("Wrapped Win32 message time broke tray dismissal".into());
    }
    // A canceled shell click must not swallow the next deliberate click.
    panel.visible = true;
    panel.deactivate(true, 1000);
    panel.tray_down(1001);
    panel.tray_down(2000);
    if !panel.tray_release(2010) {
        return Err("A canceled tray click swallowed the next activation".into());
    }
    for (key, id) in [(VK_UP.0 as u32, 1), (VK_DOWN.0 as u32, 2)] {
        for modified in [false, true] {
            if local_capture_action(key, false, modified, false, modified, modified, true).is_some()
                || local_capture_action(key, false, modified, false, modified, modified, false)
                    != Some(id)
                || local_capture_action(key, true, false, false, false, modified, false).is_some()
                || local_capture_action(key, false, false, true, false, modified, false).is_some()
            {
                return Err("Local and global tray shortcuts can dispatch a capture twice".into());
            }
        }
        if local_capture_action(key, false, false, false, false, true, true) != Some(id) {
            return Err("Bare panel arrows stopped working with Ctrl+Alt global shortcuts".into());
        }
    }
    Ok(())
}
fn update_policy_test() -> Result<(), String> {
    for mask in 0..32 {
        if update_idle(
            mask & 1 != 0,
            mask & 2 != 0,
            usize::from(mask & 4 != 0),
            mask & 8 != 0,
            mask & 16 != 0,
        ) != (mask == 0)
        {
            return Err("Automatic update can interrupt a capture, worker, popup, or exit".into());
        }
    }
    for (status, enabled) in [
        (UpdateStatus::NotConfigured, false),
        (UpdateStatus::Idle, true),
        (UpdateStatus::Checking, false),
        (UpdateStatus::UpToDate, true),
        (UpdateStatus::Ready("2.5.0".into()), true),
        (UpdateStatus::Installing("2.5.0".into()), false),
        (UpdateStatus::Failed("Offline".into()), true),
    ] {
        if update_action_enabled(&status) != enabled {
            return Err("Tray update control permits duplicate or unconfigured work".into());
        }
    }
    let disabled = Updates::new(true);
    if disabled.config.is_some()
        || disabled.next_check.is_some()
        || disabled.checking
        || disabled.prepared.is_some()
        || disabled.applying
        || disabled.apply.is_some()
        || disabled.exit_for_update
        || !matches!(disabled.status, UpdateStatus::NotConfigured)
        || UPDATE_STARTUP_DELAY != Duration::from_secs(10)
        || UPDATE_CHECK_INTERVAL != Duration::from_secs(6 * 60 * 60)
    {
        return Err("Diagnostic mode unexpectedly enabled background updates".into());
    }
    Ok(())
}
fn panel_native_test() -> Result<(), String> {
    // A hidden diagnostic host exercises native rendering and actual event
    // dispatch without creating tray icons, taking screenshots, or writing settings.
    let host = Window::new(
        "Still panel diagnostics",
        r(0., 0., native::PANEL_WIDTH, native::PANEL_HEIGHT),
        true,
    )?;
    native::style_panel(host.handle);
    unsafe {
        let extended = GetWindowLongW(host.handle, GWL_EXSTYLE) as u32;
        if extended & WS_EX_TOOLWINDOW.0 == 0
            || extended & WS_EX_APPWINDOW.0 != 0
            || GetWindowLongW(host.handle, GWL_STYLE) as u32 & WS_POPUP.0 == 0
            || IsWindowVisible(host.handle).as_bool()
        {
            return Err("Tray panel created a visible app/taskbar window".into());
        }
    }
    let (sender, receiver) = mpsc::channel();
    let mut app = App {
        hotkeys: None,
        tray: None,
        host,
        panel: PanelState {
            visible: true,
            tray_press: None,
        },
        panel_scale: 1.,
        capture: None,
        toast: None,
        previous: HWND::default(),
        receiver,
        sender,
        generation: 0,
        image_workers: 0,
        capturing: false,
        pending_mode: CaptureMode::Copy,
        deadline: None,
        ui_test: true,
        modified: false,
        login: false,
        error: None,
        shortcut_error: None,
        updates: Updates::disabled(),
        dirty: true,
        next_frame: Instant::now(),
        rate: 60.,
        hover: None,
        focus: None,
        pressed: None,
        quit: false,
    };
    app.draw_panel()?;
    for id in [1, 2, 10, 11, 12, 5, 1] {
        app.event(Event::Key(VK_TAB.0 as u32, false, false, false));
        if app.focus != Some(id) {
            return Err("Tray panel keyboard focus order failed".into());
        }
    }
    app.event(Event::Key(VK_TAB.0 as u32, false, false, true));
    if app.focus != Some(5) {
        return Err("Tray panel reverse keyboard focus failed".into());
    }
    app.focus = Some(12);
    app.event(Event::Key(VK_RETURN.0 as u32, false, false, false));
    app.focus = Some(10);
    app.event(Event::Key(VK_SPACE.0 as u32, false, false, false));
    if !app.modified || !app.login || app.capturing || app.capture.is_some() {
        return Err("Tray panel preference activation triggered the wrong action".into());
    }
    app.error =
        Some("A diagnostic error — Unicode status and modified keycaps remain readable.".into());
    app.draw_panel()?;
    app.event(Event::Deactivate(app.host.handle, Point::ZERO, 0));
    if app.panel.visible || !app.host.buttons.is_empty() {
        return Err("Tray panel stayed interactive after deactivation".into());
    }
    app.panel.visible = true;
    app.host.buttons = panel_buttons(app.modified, app.login, 1., &app.updates.status);
    app.event(Event::Key(VK_ESCAPE.0 as u32, false, false, false));
    if app.panel.visible || !app.host.buttons.is_empty() {
        return Err("Escape did not dismiss the tray panel".into());
    }
    app.updates.status = UpdateStatus::Idle;
    app.panel.visible = true;
    app.focus = None;
    app.draw_panel()?;
    for id in [1, 2, 10, 11, 12, 15, 5] {
        app.event(Event::Key(VK_TAB.0 as u32, false, false, false));
        if app.focus != Some(id) {
            return Err("Tray update action is missing from keyboard focus order".into());
        }
    }
    app.activate(15);
    app.updates.next_check = Some(Instant::now());
    app.poll_updates(Instant::now() + UPDATE_STARTUP_DELAY);
    if app.updates.checking || app.updates.applying || app.updates.apply.is_some() || app.quit {
        return Err("Diagnostic mode started an update network/apply worker".into());
    }
    // Update replies are independent of capture generations and worker counts.
    app.generation = 7;
    app.image_workers = 1;
    app.updates.checking = true;
    app.sender
        .send(Reply::UpdateChecked(Ok(None)))
        .map_err(|error| error.to_string())?;
    app.replies();
    if !matches!(app.updates.status, UpdateStatus::UpToDate)
        || app.updates.checking
        || app.image_workers != 1
        || app.updates.next_check.is_none()
    {
        return Err("A capture generation invalidated an independent update check".into());
    }
    app.sender
        .send(Reply::Exported(6, Err("Canceled diagnostic export".into())))
        .map_err(|error| error.to_string())?;
    app.replies();
    if app.image_workers != 0 || app.capture.is_some() || app.capturing {
        return Err("Canceled image workers did not release deferred updates".into());
    }
    app.updates.applying = true;
    app.sender
        .send(Reply::UpdateApplyReady(Err(
            "Diagnostic helper failure".into()
        )))
        .map_err(|error| error.to_string())?;
    app.replies();
    if app.updates.applying
        || app.updates.apply.is_some()
        || app.updates.next_check.is_none()
        || !matches!(app.updates.status, UpdateStatus::Failed(_))
        || app.quit
    {
        return Err(
            "Failed update helper interrupted the running utility or lost its retry".into(),
        );
    }
    Ok(())
}
pub fn run() {
    let diagnostic = std::env::args().any(|arg| arg == "--self-test");
    let result = if diagnostic { self_test() } else { execute() };
    if let Err(error) = result {
        if diagnostic {
            eprintln!("Still self-test failed: {error}");
            std::process::exit(1);
        } else {
            let text = native::wide(&error);
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    w!("Still"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    } else if diagnostic {
        println!("Still Windows self-test passed");
    }
}
