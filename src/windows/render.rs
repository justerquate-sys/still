//! Direct2D textures and DirectWrite typography. Frozen pixels are uploaded once;
//! selection and zoom change geometry, never resample screenshots on the CPU.
use crate::{
    coordinates::{Point, Rect},
    drawing::{Mark, Tool},
    portable_image::Image,
};
use std::collections::HashMap;
use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{Direct2D::Common::*, Direct2D::*, DirectWrite::*, Dxgi::Common::*},
    },
    core::{Interface, Result, w},
};
use windows_numerics::{Matrix3x2, Vector2};
pub const BG: u32 = 0x151617;
pub const DEEP: u32 = 0x111213;
pub const SURFACE: u32 = 0x1f2022;
pub const RAISED: u32 = 0x2b2d30;
pub const TEXT: u32 = 0xeeefec;
pub const SECONDARY: u32 = 0xb0b2b5;
pub const MUTED: u32 = 0x92959a;
pub const LIME: u32 = 0xdbf4a0;
pub const INK: u32 = 0x1b2115;
pub const ERROR: u32 = 0xf1a097;
pub fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::new(Point::new(x, y), crate::coordinates::Size::new(w, h))
}
fn native(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.origin.x as f32,
        top: r.origin.y as f32,
        right: (r.origin.x + r.size.width) as f32,
        bottom: (r.origin.y + r.size.height) as f32,
    }
}
fn color(hex: u32, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 255) as f32 / 255.,
        g: ((hex >> 8) & 255) as f32 / 255.,
        b: (hex & 255) as f32 / 255.,
        a: alpha,
    }
}
fn matrix(scale: f32, x: f32, y: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: scale,
        M12: 0.,
        M21: 0.,
        M22: scale,
        M31: x,
        M32: y,
    }
}
pub struct Renderer {
    target: ID2D1HwndRenderTarget,
    factory: ID2D1Factory,
    write: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
    fonts: HashMap<(u32, bool), IDWriteTextFormat>,
    texture: Option<(usize, ID2D1Bitmap)>,
    stamps: HashMap<usize, (Image, ID2D1Bitmap)>,
    round: ID2D1StrokeStyle,
    flat: ID2D1StrokeStyle,
}
impl Renderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.,
                dpiY: 96.,
                ..Default::default()
            };
            // The high-resolution scheduler paces frames. Waiting for vblank here
            // would add another blocking wait and can halve the effective rate.
            // DWM still composites this window at the monitor's refresh rate.
            let target = factory.CreateHwndRenderTarget(
                &props,
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U {
                        width: width.max(1),
                        height: height.max(1),
                    },
                    presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
                },
            )?;
            let brush = target.CreateSolidColorBrush(&color(TEXT, 1.), None)?;
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.,
                    ..Default::default()
                },
                None,
            )?;
            let flat = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_FLAT,
                    endCap: D2D1_CAP_STYLE_FLAT,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.,
                    ..Default::default()
                },
                None,
            )?;
            Ok(Self {
                target,
                factory,
                write,
                brush,
                fonts: HashMap::new(),
                texture: None,
                stamps: HashMap::new(),
                round,
                flat,
            })
        }
    }
    pub fn resize(&self, w: u32, h: u32) -> Result<()> {
        unsafe {
            self.target.Resize(&D2D_SIZE_U {
                width: w.max(1),
                height: h.max(1),
            })
        }
    }
    pub fn begin(&self, bg: u32) {
        unsafe {
            self.target.BeginDraw();
            self.target.SetTransform(&matrix(1., 0., 0.));
            self.target.Clear(Some(&color(bg, 1.)));
        }
    }
    pub fn end(&self) -> Result<()> {
        unsafe { self.target.EndDraw(None, None) }
    }
    pub fn scale(&self, scale: f64) {
        unsafe {
            self.target.SetTransform(&matrix(scale as f32, 0., 0.));
        }
    }
    pub fn fill(&self, rect: Rect, radius: f64, ink: u32) {
        self.fill_alpha(rect, radius, ink, 1.);
    }
    pub fn fill_alpha(&self, rect: Rect, radius: f64, ink: u32, alpha: f32) {
        unsafe {
            self.brush.SetColor(&color(ink, alpha));
            self.target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: native(rect),
                    radiusX: radius as f32,
                    radiusY: radius as f32,
                },
                &self.brush,
            );
        }
    }
    pub fn outline(&self, rect: Rect, radius: f64, ink: u32, width: f32) {
        unsafe {
            self.brush.SetColor(&color(ink, 1.));
            self.target.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: native(rect),
                    radiusX: radius as f32,
                    radiusY: radius as f32,
                },
                &self.brush,
                width,
                None,
            );
        }
    }
    pub fn line(&self, a: Point, b: Point, ink: u32, width: f32) {
        unsafe {
            self.brush.SetColor(&color(ink, 1.));
            self.target.DrawLine(
                Vector2 {
                    X: a.x as f32,
                    Y: a.y as f32,
                },
                Vector2 {
                    X: b.x as f32,
                    Y: b.y as f32,
                },
                &self.brush,
                width,
                Some(&self.round),
            );
        }
    }
    pub fn text(&mut self, text: &str, rect: Rect, size: u32, medium: bool, ink: u32) {
        unsafe {
            if !self.fonts.contains_key(&(size, medium))
                && let Ok(font) = self.write.CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    if medium {
                        DWRITE_FONT_WEIGHT_MEDIUM
                    } else {
                        DWRITE_FONT_WEIGHT_NORMAL
                    },
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size as f32,
                    w!(""),
                )
            {
                let _ = font.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
                self.fonts.insert((size, medium), font);
            }
            if let Some(font) = self.fonts.get(&(size, medium)) {
                self.brush.SetColor(&color(ink, 1.));
                let utf16: Vec<_> = text.encode_utf16().collect();
                self.target.DrawText(
                    &utf16,
                    font,
                    &native(rect),
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }
    pub fn image(&mut self, image: &Image, rect: Rect) -> Result<()> {
        unsafe {
            if self
                .texture
                .as_ref()
                .is_none_or(|(id, _)| *id != image.identity())
            {
                let maximum = self.target.GetMaximumBitmapSize() as usize;
                if image.width() > maximum || image.height() > maximum {
                    return Err(windows::core::Error::new(
                        windows::core::HRESULT(0x80070057u32 as i32),
                        "Screenshot exceeds this GPU's texture size",
                    ));
                }
                let bitmap = self.target.CreateBitmap(
                    D2D_SIZE_U {
                        width: image.width() as u32,
                        height: image.height() as u32,
                    },
                    Some(image.bgra().as_ptr().cast()),
                    (image.width() * 4) as u32,
                    &D2D1_BITMAP_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: 96.,
                        dpiY: 96.,
                    },
                )?;
                self.texture = Some((image.identity(), bitmap));
            }
            self.target.DrawBitmap(
                &self.texture.as_ref().unwrap().1,
                Some(&native(rect)),
                1.,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
            Ok(())
        }
    }
    pub fn mark(&mut self, mark: &Mark, canvas: Rect, image: &Image) -> Result<()> {
        unsafe {
            if !mark.valid() {
                return Ok(());
            }
            let scale = (canvas.size.width / image.width() as f64) as f32;
            self.target.PushAxisAlignedClip(
                &native(canvas),
                if mark.tool.is_pixelation() {
                    D2D1_ANTIALIAS_MODE_ALIASED
                } else {
                    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE
                },
            );
            self.target.SetTransform(&matrix(
                scale,
                canvas.origin.x as f32,
                canvas.origin.y as f32,
            ));
            let result = self.mark_pixels(mark);
            self.target.SetTransform(&matrix(1., 0., 0.));
            self.target.PopAxisAlignedClip();
            result
        }
    }
    unsafe fn mark_pixels(&mut self, mark: &Mark) -> Result<()> {
        unsafe {
            if mark.tool == Tool::Text || mark.tool.is_pixelation() {
                let Some(stamp) = &mark.stamp else {
                    return Ok(());
                };
                let bitmap = self.stamp_bitmap(&stamp.pixels)?;
                let p = mark.points[0];
                if mark.tool == Tool::Text {
                    self.target.DrawBitmap(
                        &bitmap,
                        Some(&native(r(
                            p.x,
                            p.y,
                            stamp.pixels.width() as f64,
                            stamp.pixels.height() as f64,
                        ))),
                        1.,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                    return Ok(());
                }
                let last = *mark.points.last().unwrap();
                let mask: ID2D1Geometry = if mark.tool == Tool::PixelateArea {
                    let left = p.x.min(last.x).floor();
                    let top = p.y.min(last.y).floor();
                    self.factory
                        .CreateRectangleGeometry(&native(r(
                            left,
                            top,
                            p.x.max(last.x).ceil() - left,
                            p.y.max(last.y).ceil() - top,
                        )))?
                        .cast()?
                } else if mark.points.iter().all(|q| q == &p) {
                    self.factory
                        .CreateEllipseGeometry(&D2D1_ELLIPSE {
                            point: Vector2 {
                                X: p.x as f32,
                                Y: p.y as f32,
                            },
                            radiusX: mark.width as f32,
                            radiusY: mark.width as f32,
                        })?
                        .cast()?
                } else {
                    let path = self.factory.CreatePathGeometry()?;
                    let sink = path.Open()?;
                    sink.BeginFigure(
                        Vector2 {
                            X: p.x as f32,
                            Y: p.y as f32,
                        },
                        D2D1_FIGURE_BEGIN_HOLLOW,
                    );
                    for q in &mark.points[1..] {
                        sink.AddLine(Vector2 {
                            X: q.x as f32,
                            Y: q.y as f32,
                        });
                    }
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    sink.Close()?;
                    let widened = self.factory.CreatePathGeometry()?;
                    let sink = widened.Open()?;
                    path.Widen(mark.width as f32 * 2., &self.round, None, 0.25, &sink)?;
                    sink.Close()?;
                    widened.cast()?
                };
                let mut parameters = D2D1_LAYER_PARAMETERS {
                    contentBounds: D2D_RECT_F {
                        left: -f32::MAX,
                        top: -f32::MAX,
                        right: f32::MAX,
                        bottom: f32::MAX,
                    },
                    geometricMask: std::mem::ManuallyDrop::new(Some(mask)),
                    // Opaque synthetic pixels replace the screenshot at every
                    // covered sample; mask edges cannot mix original content.
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_ALIASED,
                    maskTransform: matrix(1., 0., 0.),
                    opacity: 1.,
                    ..Default::default()
                };
                self.target.PushLayer(&parameters, None);
                self.target.DrawBitmap(
                    &bitmap,
                    Some(&native(r(
                        stamp.origin.x,
                        stamp.origin.y,
                        stamp.pixels.width() as f64,
                        stamp.pixels.height() as f64,
                    ))),
                    1.,
                    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                );
                self.target.PopLayer();
                std::mem::ManuallyDrop::drop(&mut parameters.geometricMask);
                return Ok(());
            }
            let high = mark.tool == Tool::Highlighter;
            let c = mark.color.map(|c| c.clamp(0., 1.) as f32);
            self.brush.SetColor(&D2D1_COLOR_F {
                r: c[0],
                g: c[1],
                b: c[2],
                a: c[3] * if high { 0.3 } else { 1. },
            });
            let width = mark.width as f32 * if high { 6. } else { 1. };
            let first = mark.points[0];
            let last = *mark.points.last().unwrap();
            if matches!(mark.tool, Tool::Pen | Tool::Highlighter)
                && mark.points.iter().all(|p| p == &first)
            {
                if !high {
                    self.brush.SetColor(&D2D1_COLOR_F {
                        r: 0.035,
                        g: 0.04,
                        b: 0.045,
                        a: 0.9,
                    });
                    self.target.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: Vector2 {
                                X: first.x as f32,
                                Y: first.y as f32,
                            },
                            radiusX: width * 0.9,
                            radiusY: width * 0.9,
                        },
                        &self.brush,
                    );
                    self.brush.SetColor(&D2D1_COLOR_F {
                        r: c[0],
                        g: c[1],
                        b: c[2],
                        a: c[3],
                    });
                }
                self.target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 {
                            X: first.x as f32,
                            Y: first.y as f32,
                        },
                        radiusX: width / 2.,
                        radiusY: width / 2.,
                    },
                    &self.brush,
                );
                return Ok(());
            }
            let geometry = self.factory.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            sink.BeginFigure(
                Vector2 {
                    X: first.x as f32,
                    Y: first.y as f32,
                },
                D2D1_FIGURE_BEGIN_HOLLOW,
            );
            let add = |p: Point| {
                sink.AddLine(Vector2 {
                    X: p.x as f32,
                    Y: p.y as f32,
                })
            };
            match mark.tool {
                Tool::Rectangle => {
                    add(Point::new(last.x, first.y));
                    add(last);
                    add(Point::new(first.x, last.y));
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                }
                Tool::Arrow => {
                    add(last);
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    if let Some(head) = mark.arrow_head() {
                        sink.BeginFigure(
                            Vector2 {
                                X: head[0].x as f32,
                                Y: head[0].y as f32,
                            },
                            D2D1_FIGURE_BEGIN_HOLLOW,
                        );
                        add(head[1]);
                        add(head[2]);
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                }
                _ => {
                    for p in &mark.points[1..] {
                        add(*p);
                    }
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                }
            }
            sink.Close()?;
            if !high {
                self.brush.SetColor(&D2D1_COLOR_F {
                    r: 0.035,
                    g: 0.04,
                    b: 0.045,
                    a: 0.9,
                });
                self.target
                    .DrawGeometry(&geometry, &self.brush, width * 1.8, Some(&self.round));
                self.brush.SetColor(&D2D1_COLOR_F {
                    r: c[0],
                    g: c[1],
                    b: c[2],
                    a: c[3],
                });
            }
            self.target.DrawGeometry(
                &geometry,
                &self.brush,
                width,
                Some(if high { &self.flat } else { &self.round }),
            );
            Ok(())
        }
    }
    fn stamp_bitmap(&mut self, image: &Image) -> Result<ID2D1Bitmap> {
        unsafe {
            if let Some((_, bitmap)) = self.stamps.get(&image.identity()) {
                return Ok(bitmap.clone());
            }
            let bitmap = self.target.CreateBitmap(
                D2D_SIZE_U {
                    width: image.width() as u32,
                    height: image.height() as u32,
                },
                Some(image.bgra().as_ptr().cast()),
                (image.width() * 4) as u32,
                &D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.,
                    dpiY: 96.,
                },
            )?;
            if self.stamps.len() >= 16 {
                self.stamps.clear();
            }
            self.stamps
                .insert(image.identity(), (image.clone(), bitmap.clone()));
            Ok(bitmap)
        }
    }
    pub fn logo(&self, x: f64, y: f64, size: f64) {
        self.fill(r(x, y, size, size), size * 0.24, LIME);
        for (cx, cy, dx, dy) in [
            (0.26, 0.26, 1., 1.),
            (0.74, 0.26, -1., 1.),
            (0.26, 0.74, 1., -1.),
            (0.74, 0.74, -1., -1.),
        ] {
            let (a, b) = (x + size * cx, y + size * cy);
            self.line(
                Point::new(a + dx * size * 0.13, b),
                Point::new(a, b),
                INK,
                (size * 0.048) as f32,
            );
            self.line(
                Point::new(a, b),
                Point::new(a, b + dy * size * 0.13),
                INK,
                (size * 0.048) as f32,
            );
        }
        for dx in [0.46, 0.54] {
            self.line(
                Point::new(x + size * dx, y + size * 0.42),
                Point::new(x + size * dx, y + size * 0.58),
                INK,
                (size * 0.048) as f32,
            );
        }
    }
}
