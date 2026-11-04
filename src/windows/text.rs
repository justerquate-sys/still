//! Native editable text gives keyboard layouts, IMEs, paste and dictation a
//! standard insertion target. Committed glyphs become one immutable annotation.
use super::{Event, enqueue, native};
use crate::{
    coordinates::{Rect, Size},
    drawing::Stamp,
    portable_image::Image,
};
use std::{cell::Cell, sync::Arc};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::Common::*, Direct2D::*, DirectWrite::*, Dxgi::Common::*, Gdi::*, Imaging::*,
        },
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::{Controls::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};
thread_local! {pub static INK:Cell<u32>=const{Cell::new(0x003333ff)};}
pub struct Input {
    pub handle: HWND,
    font: HFONT,
}
impl Input {
    pub fn new(parent: HWND, scale: f64) -> Result<Self, String> {
        unsafe {
            let handle = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                None,
                WS_CHILD
                    | WS_VISIBLE
                    | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                Some(GetModuleHandleW(None).map_err(|e| e.to_string())?.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            if !SetWindowSubclass(handle, Some(edit_proc), 1, 0).as_bool() {
                let _ = DestroyWindow(handle);
                return Err("Could not initialize text input".into());
            }
            let mut face = [0u16; 32];
            let name = native::wide("Segoe UI");
            face[..name.len()].copy_from_slice(&name);
            let font = CreateFontIndirectW(&LOGFONTW {
                lfHeight: -(24. * scale).round() as i32,
                lfWeight: 600,
                lfFaceName: face,
                ..Default::default()
            });
            SendMessageW(
                handle,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
            SendMessageW(handle, EM_SETLIMITTEXT, Some(WPARAM(8192)), Some(LPARAM(0)));
            Ok(Self { handle, font })
        }
    }
    pub fn value(&self) -> String {
        unsafe {
            let len = GetWindowTextLengthW(self.handle).max(0) as usize;
            let mut buf = vec![0u16; len + 1];
            let count = GetWindowTextW(self.handle, &mut buf);
            String::from_utf16_lossy(&buf[..count.max(0) as usize])
        }
    }
    pub fn clear(&self) {
        unsafe {
            let _ = SetWindowTextW(self.handle, w!(""));
        }
    }
    pub fn focus(&self) {
        unsafe {
            let _ = SetFocus(Some(self.handle));
        }
    }
    pub fn content_size(&self, max_width: f64, max_height: f64, scale: f64) -> Size {
        unsafe {
            let dc = GetDC(Some(self.handle));
            let old = SelectObject(dc, self.font.into());
            let mut text = native::wide(&self.value());
            text.pop();
            let inset = (8. * scale).round() as i32;
            let mut rect = RECT {
                right: (max_width - inset as f64).max(1.) as i32,
                bottom: 4096,
                ..Default::default()
            };
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
            );
            SelectObject(dc, old);
            ReleaseDC(Some(self.handle), dc);
            let grid = 4. * scale;
            Size::new(
                (((rect.right + inset) as f64 / grid).ceil() * grid)
                    .max(96. * scale)
                    .min(max_width),
                ((((rect.bottom + inset) as f64) / grid).ceil() * grid)
                    .max(40. * scale)
                    .min(max_height),
            )
        }
    }
    pub fn position(&self, rect: Rect, visible: bool, scale: f64) {
        unsafe {
            // Empty input remains focused and accessible to dictation while taking
            // one pixel, so it never blocks pen/brush hit testing over the canvas.
            let (w, h) = if visible {
                (rect.size.width as i32, rect.size.height as i32)
            } else {
                (1, 1)
            };
            let _ = SetWindowPos(
                self.handle,
                Some(HWND_TOP),
                rect.origin.x as i32,
                rect.origin.y as i32,
                w.max(1),
                h.max(1),
                SWP_NOACTIVATE,
            );
            let inset = (4. * scale).round() as i32;
            let content = RECT {
                left: inset,
                top: inset,
                right: (w - inset).max(inset + 1),
                bottom: (h - inset).max(inset + 1),
            };
            SendMessageW(
                self.handle,
                EM_SETRECT,
                Some(WPARAM(0)),
                Some(LPARAM((&content as *const RECT) as isize)),
            );
        }
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.handle, Some(edit_proc), 1);
            let _ = DestroyWindow(self.handle);
            let _ = DeleteObject(self.font.into());
        }
    }
}
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        if msg == WM_KEYDOWN {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let repeat = lp.0 & (1 << 30) != 0;
            let empty = GetWindowTextLengthW(hwnd) == 0;
            if repeat && matches!(wp.0, 13 | 27) {
                return LRESULT(0);
            }
            if wp.0 == VK_RETURN.0 as usize && !shift {
                enqueue(Event::TextFinish(empty || ctrl));
                return LRESULT(0);
            }
            if wp.0 == VK_ESCAPE.0 as usize {
                enqueue(if empty {
                    Event::Key(VK_ESCAPE.0 as u32, false, ctrl, shift)
                } else {
                    Event::TextDiscard
                });
                return LRESULT(0);
            }
            if matches!(wp.0, 9 | 38 | 40) || (empty && ctrl && matches!(wp.0, 67 | 90)) {
                enqueue(Event::Key(wp.0 as u32, repeat, ctrl, shift));
                return LRESULT(0);
            }
            if ctrl && wp.0 == b'A' as usize {
                SendMessageW(hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                return LRESULT(0);
            }
        }
        if msg == WM_CHAR
            && ((wp.0 == 13 && GetKeyState(VK_SHIFT.0 as i32) >= 0) || wp.0 == 27 || wp.0 == 9)
        {
            return LRESULT(0);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
pub fn stamp(
    text: &str,
    color: [f64; 4],
    scale: f64,
    max_width: f64,
    max_height: f64,
) -> Result<Arc<Stamp>, String> {
    unsafe {
        if text.trim().is_empty() || text.chars().count() > 4096 {
            return Err("Text must contain 1–4096 characters".into());
        }
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|e| e.to_string())?;
        let write: IDWriteFactory =
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| e.to_string())?;
        let size = (24. * scale) as f32;
        let pad = (size * 0.15).ceil();
        let layout_width = (max_width as f32).clamp(size * 3., 4096.);
        let format = write
            .CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )
            .map_err(|e| e.to_string())?;
        let utf16: Vec<_> = text.encode_utf16().collect();
        let layout = write
            .CreateTextLayout(&utf16, &format, layout_width - pad * 2., 4096. - pad * 2.)
            .map_err(|e| e.to_string())?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics).map_err(|e| e.to_string())?;
        let width =
            (metrics.widthIncludingTrailingWhitespace.ceil() + pad * 2.).clamp(1., 4096.) as u32;
        let height = (metrics.height.ceil() + pad * 2.).clamp(1., 4096.) as u32;
        if metrics.height.ceil() + pad * 2. > max_height as f32 || width as f64 > max_width + 1. {
            return Err("Text does not fit here. Shorten it or choose a larger area".into());
        }
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
        let bitmap = wic
            .CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )
            .map_err(|e| e.to_string())?;
        let target = factory
            .CreateWicBitmapRenderTarget(
                &bitmap,
                &D2D1_RENDER_TARGET_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.,
                    dpiY: 96.,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let black = D2D1_COLOR_F {
            r: 0.035,
            g: 0.04,
            b: 0.045,
            a: 1.,
        };
        let brush = target
            .CreateSolidColorBrush(&black, None)
            .map_err(|e| e.to_string())?;
        target.BeginDraw();
        target.Clear(Some(&D2D1_COLOR_F::default()));
        let outline = (size * 0.04).max(1.);
        for (dx, dy) in [
            (-1., -1.),
            (0., -1.),
            (1., -1.),
            (-1., 0.),
            (1., 0.),
            (-1., 1.),
            (0., 1.),
            (1., 1.),
        ] {
            target.DrawTextLayout(
                windows_numerics::Vector2 {
                    X: pad + dx * outline,
                    Y: pad + dy * outline,
                },
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        brush.SetColor(&D2D1_COLOR_F {
            r: color[0] as f32,
            g: color[1] as f32,
            b: color[2] as f32,
            a: 1.,
        });
        target.DrawTextLayout(
            windows_numerics::Vector2 { X: pad, Y: pad },
            &layout,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
        target.EndDraw(None, None).map_err(|e| e.to_string())?;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        bitmap
            .CopyPixels(std::ptr::null(), width * 4, &mut rgba)
            .map_err(|e| e.to_string())?;
        for p in rgba.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
        }
        Stamp::new(Image::from_premultiplied_rgba(
            width as usize,
            height as usize,
            rgba,
        )?)
    }
}
