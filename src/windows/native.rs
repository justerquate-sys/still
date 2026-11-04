//! Small RAII wrappers around Windows facilities. No hooks, injected code,
//! continuous recording, administrator privileges, or browser runtime.
use crate::{
    coordinates::{Point, Rect, Size},
    portable_image::{self, EncodedImage, Image},
    refresh,
};
use std::{mem::size_of, ptr, sync::mpsc::Sender, time::Instant};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{
            DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*, Registry::*, Threading::*,
        },
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, Result, w},
};
pub const WAKE: u32 = WM_APP + 1;
pub const TRAY: u32 = WM_APP + 2;
pub const SHOW: u32 = WM_APP + 3;
pub const PANEL_WIDTH: f64 = 340.;
pub const PANEL_HEIGHT: f64 = 408.;
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub fn rect(r: RECT) -> Rect {
    Rect::new(
        Point::new(r.left as f64, r.top as f64),
        Size::new((r.right - r.left) as f64, (r.bottom - r.top) as f64),
    )
}
pub fn desktop() -> Rect {
    unsafe {
        Rect::new(
            Point::new(
                GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            ),
            Size::new(
                GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
            ),
        )
    }
}
pub fn cursor() -> POINT {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    point
}
pub fn monitor(point: POINT) -> (Rect, f64) {
    unsafe {
        let handle = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !GetMonitorInfoW(handle, &mut info.monitorInfo).as_bool() {
            return (desktop(), 60.);
        }
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let hz = if EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            mode.dmDisplayFrequency as f64
        } else {
            60.
        };
        (rect(info.monitorInfo.rcMonitor), refresh::target_hz(hz))
    }
}
pub fn display_scale(point: POINT) -> f64 {
    unsafe {
        let mut x = 96;
        let mut y = 96;
        let _ = GetDpiForMonitor(
            MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST),
            MDT_EFFECTIVE_DPI,
            &mut x,
            &mut y,
        );
        x.max(96) as f64 / 96.0
    }
}
fn work_area(point: POINT) -> Rect {
    unsafe {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            rect(info.rcWork)
        } else {
            monitor(point).0
        }
    }
}
/// Place the panel beside the notification icon, including taskbars at any edge.
/// Geometry is in physical pixels; the returned scale also fits small work areas.
pub fn panel_bounds(anchor: Rect, work: Rect, dpi_scale: f64) -> (Rect, f64) {
    let margin = (8. * dpi_scale)
        .min(work.size.width / 8.)
        .min(work.size.height / 8.);
    let scale = dpi_scale
        .min((work.size.width - margin * 2.) / PANEL_WIDTH)
        .min((work.size.height - margin * 2.) / PANEL_HEIGHT)
        .max(f64::EPSILON);
    let width = (PANEL_WIDTH * scale).round().max(1.);
    let height = (PANEL_HEIGHT * scale).round().max(1.);
    let gap = 8. * scale;
    let right = work.origin.x + work.size.width;
    let bottom = work.origin.y + work.size.height;
    let ax = anchor.origin.x + anchor.size.width / 2.;
    let ay = anchor.origin.y + anchor.size.height / 2.;
    let mut x = ax - width / 2.;
    let mut y = if ay >= work.origin.y + work.size.height / 2. {
        anchor.origin.y - gap - height
    } else {
        anchor.origin.y + anchor.size.height + gap
    };
    if anchor.origin.x + anchor.size.width <= work.origin.x {
        x = anchor.origin.x + anchor.size.width + gap;
        y = ay - height / 2.;
    } else if anchor.origin.x >= right {
        x = anchor.origin.x - gap - width;
        y = ay - height / 2.;
    }
    x = x.clamp(
        work.origin.x + margin,
        (right - margin - width).max(work.origin.x + margin),
    );
    y = y.clamp(
        work.origin.y + margin,
        (bottom - margin - height).max(work.origin.y + margin),
    );
    (
        Rect::new(Point::new(x.round(), y.round()), Size::new(width, height)),
        scale,
    )
}
pub fn style_panel(hwnd: HWND) {
    unsafe {
        let corner = DWMWCP_ROUND;
        let dark = windows::core::BOOL(1);
        // Windows 11 supplies native corners; older versions retain a compact
        // borderless tool window. These hints never affect capture overlays.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const windows::core::BOOL).cast(),
            size_of::<windows::core::BOOL>() as u32,
        );
    }
}
pub fn reduce_motion() -> bool {
    unsafe {
        let mut enabled = windows::core::BOOL(1);
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        !enabled.as_bool()
    }
}
pub struct BrushCursor(pub HCURSOR);
impl BrushCursor {
    pub fn new(scale: f64) -> Result<Self> {
        unsafe {
            let size = ((28. * scale / 16.).ceil() as usize * 16).clamp(32, 128);
            let stride = size / 8;
            let mut and = vec![255u8; stride * size];
            let mut xor = vec![0u8; stride * size];
            let mid = size as f64 / 2.;
            for y in 0..size {
                for x in 0..size {
                    let distance =
                        ((x as f64 + 0.5 - mid).hypot(y as f64 + 0.5 - mid) - 12. * scale).abs();
                    if distance <= 1.5 * scale {
                        let i = y * stride + x / 8;
                        let bit = 0x80 >> (x % 8);
                        and[i] &= !bit;
                        if distance <= 0.5 * scale {
                            xor[i] |= bit;
                        }
                    }
                }
            }
            CreateCursor(
                None,
                size as i32 / 2,
                size as i32 / 2,
                size as i32,
                size as i32,
                and.as_ptr().cast(),
                xor.as_ptr().cast(),
            )
            .map(Self)
        }
    }
}
impl Drop for BrushCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyCursor(self.0);
        }
    }
}
struct CaptureDc {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl Drop for CaptureDc {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.memory.is_invalid() {
                let _ = DeleteDC(self.memory);
            }
            if !self.screen.is_invalid() {
                ReleaseDC(None, self.screen);
            }
        }
    }
}
pub fn capture(bounds: Rect) -> std::result::Result<Image, String> {
    unsafe {
        let (width, height) = (bounds.size.width as usize, bounds.size.height as usize);
        portable_image::validate_dimensions(width, height)?;
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err("Windows could not access the desktop".into());
        }
        let mut dc = CaptureDc {
            screen,
            memory: CreateCompatibleDC(Some(screen)),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
        };
        if dc.memory.is_invalid() {
            return Err("Could not create a screenshot context".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = ptr::null_mut();
        dc.bitmap = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| e.to_string())?;
        if bits.is_null() {
            return Err("Could not allocate screenshot pixels".into());
        }
        dc.previous = SelectObject(dc.memory, dc.bitmap.into());
        BitBlt(
            dc.memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            bounds.origin.x as i32,
            bounds.origin.y as i32,
            SRCCOPY | CAPTUREBLT,
        )
        .map_err(|e| e.to_string())?;
        let _ = GdiFlush();
        let mut rgba = std::slice::from_raw_parts(bits.cast::<u8>(), width * height * 4).to_vec();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        Image::from_rgba(width, height, rgba)
    }
}
pub fn flush_desktop() {
    unsafe {
        let _ = DwmFlush();
    }
}
struct Memory(Option<HGLOBAL>);
impl Memory {
    fn new(data: &[u8]) -> Result<Self> {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, data.len())?;
            let memory = Self(Some(handle));
            let p = GlobalLock(handle);
            if p.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            ptr::copy_nonoverlapping(data.as_ptr(), p.cast(), data.len());
            let _ = GlobalUnlock(handle);
            Ok(memory)
        }
    }
    fn transfer(&mut self, format: u32) -> bool {
        unsafe {
            if let Some(handle) = self.0
                && SetClipboardData(format, Some(HANDLE(handle.0))).is_ok()
            {
                self.0 = None;
                true
            } else {
                false
            }
        }
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = GlobalFree(Some(h));
            }
        }
    }
}
struct Clipboard;
pub fn validate_clipboard_buffers(encoded: &EncodedImage) -> Result<()> {
    let _png = Memory::new(&encoded.png)?;
    let _dib = Memory::new(&encoded.dibv5)?;
    Ok(())
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}
pub fn copy(hwnd: HWND, encoded: &EncodedImage) -> std::result::Result<(), String> {
    unsafe {
        // Allocate both formats before opening/emptying the user's clipboard.
        let mut png = Memory::new(&encoded.png).map_err(|e| e.to_string())?;
        let mut dib = Memory::new(&encoded.dibv5).map_err(|e| e.to_string())?;
        let format = RegisterClipboardFormatW(w!("PNG"));
        if format == 0 {
            return Err("Windows could not register PNG clipboard data".into());
        }
        OpenClipboard(Some(hwnd))
            .map_err(|_| "The clipboard is busy. Try copying again.".to_string())?;
        let _guard = Clipboard;
        EmptyClipboard().map_err(|e| e.to_string())?;
        let dib_ok = dib.transfer(17); // CF_DIBV5, recognized by native Windows apps.
        let png_ok = png.transfer(format);
        if dib_ok || png_ok {
            Ok(())
        } else {
            Err("Windows could not write the image to the clipboard".into())
        }
    }
}
pub struct Hotkeys {
    hwnd: HWND,
    ids: Vec<i32>,
}
impl Hotkeys {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            ids: Vec::new(),
        }
    }
    pub fn register(&mut self, modified: bool) -> std::result::Result<(), String> {
        self.clear();
        let mut errors = Vec::new();
        for (id, key, mods, label) in [
            (
                1,
                VK_UP,
                if modified {
                    MOD_CONTROL | MOD_ALT
                } else {
                    HOT_KEY_MODIFIERS(0)
                },
                "Up",
            ),
            (
                2,
                VK_DOWN,
                if modified {
                    MOD_CONTROL | MOD_ALT
                } else {
                    HOT_KEY_MODIFIERS(0)
                },
                "Down",
            ),
            (3, VK_SPACE, MOD_CONTROL | MOD_ALT, "Ctrl+Alt+Space"),
        ] {
            if unsafe { RegisterHotKey(Some(self.hwnd), id, mods | MOD_NOREPEAT, key.0 as u32) }
                .is_ok()
            {
                self.ids.push(id);
            } else {
                errors.push(label);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Shortcut unavailable: {}. Choose Ctrl+Alt arrows or use the tray menu.",
                errors.join(", ")
            ))
        }
    }
    pub fn registered(&self, id: i32) -> bool {
        self.ids.contains(&id)
    }
    fn clear(&mut self) {
        for id in self.ids.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
        }
    }
}
impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.clear();
    }
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn settings_key(path: PCWSTR) -> Result<Key> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        Ok(Key(key))
    }
}
pub fn modified() -> bool {
    unsafe {
        let mut value = 0u32;
        let mut size = 4;
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Still"),
            w!("ModifiedShortcuts"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
        .is_ok()
            && value != 0
    }
}
pub fn save_modified(modified: bool) -> Result<()> {
    let key = settings_key(w!("Software\\Still"))?;
    unsafe {
        RegSetValueExW(
            key.0,
            w!("ModifiedShortcuts"),
            None,
            REG_DWORD,
            Some(&(modified as u32).to_le_bytes()),
        )
        .ok()
    }
}
const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
fn launch_command() -> std::result::Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\" --background", exe.display()))
}
pub fn login_enabled() -> bool {
    unsafe {
        let mut buf = [0u16; 32768];
        let mut size = (buf.len() * 2) as u32;
        if RegGetValueW(
            HKEY_CURRENT_USER,
            RUN,
            w!("Still"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .is_err()
        {
            return false;
        }
        let text =
            String::from_utf16_lossy(&buf[..buf.iter().position(|x| *x == 0).unwrap_or(buf.len())]);
        launch_command().is_ok_and(|command| text.eq_ignore_ascii_case(&command))
    }
}
pub fn set_login(enabled: bool) -> std::result::Result<(), String> {
    let key = settings_key(RUN).map_err(|e| e.to_string())?;
    unsafe {
        if enabled {
            let data = wide(&launch_command()?);
            let bytes = std::slice::from_raw_parts(data.as_ptr().cast(), data.len() * 2);
            RegSetValueExW(key.0, w!("Still"), None, REG_SZ, Some(bytes))
                .ok()
                .map_err(|e| e.to_string())
        } else {
            let result = RegDeleteValueW(key.0, w!("Still"));
            if result == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                result.ok().map_err(|e| e.to_string())
            }
        }
    }
}
pub struct Timer(HANDLE);
impl Timer {
    pub fn new() -> Result<Self> {
        unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .or_else(|_| CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS.0))
            .map(Self)
        }
    }
    pub fn wait(&self, deadline: Option<Instant>) {
        unsafe {
            if let Some(deadline) = deadline {
                let ticks = -((deadline
                    .saturating_duration_since(Instant::now())
                    .as_nanos()
                    / 100)
                    .max(1)
                    .min(i64::MAX as u128) as i64);
                if SetWaitableTimerEx(self.0, &ticks, 0, None, None, None, 0).is_ok() {
                    MsgWaitForMultipleObjectsEx(
                        Some(&[self.0]),
                        INFINITE,
                        QS_ALLINPUT,
                        MWMO_INPUTAVAILABLE,
                    );
                    return;
                }
            }
            MsgWaitForMultipleObjectsEx(
                None,
                if deadline.is_some() { 1 } else { INFINITE },
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        unsafe {
            let _ = CancelWaitableTimer(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
pub fn worker<T: Send + 'static>(
    sender: Sender<T>,
    hwnd: HWND,
    work: impl FnOnce() -> T + Send + 'static,
) {
    let window = hwnd.0 as usize;
    std::thread::spawn(move || {
        let result = work();
        if sender.send(result).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(window as *mut _)), WAKE, WPARAM(0), LPARAM(0));
            }
        }
    });
}
pub struct Tray {
    data: NOTIFYICONDATAW,
    icon: HICON,
}
impl Tray {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let mut pixels = vec![0u8; 32 * 32 * 4];
            for y in 2..30 {
                for x in 2..30 {
                    let i = (y * 32 + x) * 4;
                    pixels[i..i + 4].copy_from_slice(&[160, 244, 219, 255]);
                }
            }
            for (x, y) in [(8, 8), (23, 8), (8, 23), (23, 23)] {
                for d in 0..6 {
                    for (px, py) in [
                        (if x < 16 { x + d } else { x - d }, y),
                        (x, if y < 16 { y + d } else { y - d }),
                    ] {
                        let i = (py * 32 + px) * 4;
                        pixels[i..i + 4].copy_from_slice(&[21, 33, 27, 255]);
                    }
                }
            }
            for x in [14, 17] {
                for y in 13..19 {
                    let i = (y * 32 + x) * 4;
                    pixels[i..i + 4].copy_from_slice(&[21, 33, 27, 255]);
                }
            }
            let icon = if let Ok(resource) = LoadImageW(
                Some(GetModuleHandleW(None)?.into()),
                // MAKEINTRESOURCEW(1): Win32 interprets this as an integer ID,
                // not as a pointer to UTF-16 memory.
                PCWSTR(ptr::without_provenance::<u16>(1)),
                IMAGE_ICON,
                32,
                32,
                IMAGE_FLAGS(0),
            ) {
                HICON(resource.0)
            } else {
                CreateIcon(None, 32, 32, 1, 32, [0u8; 128].as_ptr(), pixels.as_ptr())?
            };
            let mut data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: TRAY,
                hIcon: icon,
                ..Default::default()
            };
            let tip = wide("Still — capture and copy");
            data.szTip[..tip.len()].copy_from_slice(&tip);
            let tray = Self { data, icon };
            tray.restore()?;
            Ok(tray)
        }
    }
    pub fn restore(&self) -> Result<()> {
        unsafe { Shell_NotifyIconW(NIM_ADD, &self.data).ok() }
    }
    pub fn icon(&self) -> HICON {
        self.icon
    }
    pub fn icon_bounds(&self) -> Option<Rect> {
        unsafe {
            let identifier = NOTIFYICONIDENTIFIER {
                cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
                hWnd: self.data.hWnd,
                uID: self.data.uID,
                ..Default::default()
            };
            Shell_NotifyIconGetRect(&identifier)
                .ok()
                .filter(|r| r.right > r.left && r.bottom > r.top)
                .map(rect)
        }
    }
    pub fn panel_placement(&self) -> (Rect, f64) {
        let anchor = self.icon_bounds().unwrap_or_else(|| {
            let point = cursor();
            Rect::new(
                Point::new(point.x as f64, point.y as f64),
                Size::new(1., 1.),
            )
        });
        let point = POINT {
            x: (anchor.origin.x + anchor.size.width / 2.) as i32,
            y: (anchor.origin.y + anchor.size.height / 2.) as i32,
        };
        panel_bounds(anchor, work_area(point), display_scale(point))
    }
    pub fn menu(&self, modified: bool, login: bool, status: Option<&str>) -> u32 {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return 0;
            };
            let title = wide(status.unwrap_or("Still · Ready to capture"));
            let _ = AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, PCWSTR(title.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            for (id, text) in [
                (1, "Capture and copy  ↑"),
                (2, "Capture and draw  ↓"),
                (10, "Launch at login"),
                (11, "Global shortcuts: ↑ / ↓"),
                (12, "Global shortcuts: Ctrl+Alt ↑ / ↓"),
            ] {
                let text = wide(text);
                let checked =
                    (id == 10 && login) || (id == 11 && !modified) || (id == 12 && modified);
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED },
                    id,
                    PCWSTR(text.as_ptr()),
                );
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, 5, w!("Quit Still"));
            let p = cursor();
            let _ = SetForegroundWindow(self.data.hWnd);
            let result = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                p.x,
                p.y,
                Some(0),
                self.data.hWnd,
                None,
            )
            .0 as u32;
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(self.data.hWnd), WM_NULL, WPARAM(0), LPARAM(0));
            result
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            let _ = DestroyIcon(self.icon);
        }
    }
}
