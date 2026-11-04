//! Immutable image ownership, original-resolution annotation, and clipboard encoders.
//! CoreGraphics composites pixels, Rust encodes PNG, and ImageIO provides TIFF.
//! Every operation in this module can run off the main thread.

use objc2_core_foundation::{CFMutableData, CFRetained, CFString};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGInterpolationQuality, CGLineCap, CGLineJoin, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_image_io::CGImageDestination;

use crate::geometry;

/// A retained, immutable CGImage. The upstream binding implements Send + Sync
/// for CGImage; no unchecked thread-safety implementation is needed here.
#[derive(Clone, Debug)]
pub struct Image(CFRetained<CGImage>);

impl Image {
    /// Adopt an already-retained immutable image after checking its dimensions.
    pub fn from_cg(image: CFRetained<CGImage>) -> Result<Self, String> {
        validate_dimensions(CGImage::width(Some(&image)), CGImage::height(Some(&image)))?;
        Ok(Self(image))
    }

    pub fn cg_image(&self) -> &CGImage {
        &self.0
    }

    pub fn width(&self) -> usize {
        CGImage::width(Some(&self.0))
    }

    pub fn height(&self) -> usize {
        CGImage::height(Some(&self.0))
    }
}

/// Bound allocations before calling platform APIs and reject integer overflow.
pub(crate) fn validate_dimensions(width: usize, height: usize) -> Result<(), String> {
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0
        || height == 0
        || width > 32768
        || height > 32768
        || bytes.is_none_or(|n| n > 512 * 1024 * 1024)
    {
        return Err("The image dimensions are unsupported (maximum 128 megapixels).".into());
    }
    Ok(())
}

pub use crate::drawing::{Mark, Tool};

impl Mark {
    /// Draw in a context whose origin is the top-left of the original crop.
    pub fn draw(&self, context: &CGContext) {
        if !self.valid() {
            return;
        }
        let c = Some(context);
        if let Some(stamp) = &self.stamp {
            if self.tool == Tool::Text {
                let p = self.points[0];
                draw_stamp(context, &stamp.native, p.x, p.y);
                return;
            }
            if self.tool.is_pixelation() {
                let first = self.points[0];
                let last = *self.points.last().unwrap();
                CGContext::save_g_state(c);
                // Redaction has binary coverage: no original pixels may show
                // through antialiased clip edges or interpolated texture alpha.
                CGContext::set_allows_antialiasing(c, false);
                CGContext::set_should_antialias(c, false);
                CGContext::set_interpolation_quality(c, CGInterpolationQuality::None);
                if self.tool == Tool::PixelateArea {
                    let left = first.x.min(last.x).floor();
                    let top = first.y.min(last.y).floor();
                    CGContext::clip_to_rect(
                        c,
                        rect(
                            left,
                            top,
                            first.x.max(last.x).ceil() - left,
                            first.y.max(last.y).ceil() - top,
                        ),
                    );
                } else if self.points.iter().all(|p| p == &first) {
                    CGContext::begin_path(c);
                    CGContext::add_ellipse_in_rect(
                        c,
                        rect(
                            first.x - self.width,
                            first.y - self.width,
                            self.width * 2.,
                            self.width * 2.,
                        ),
                    );
                    CGContext::clip(c);
                } else {
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, first.x, first.y);
                    for p in &self.points[1..] {
                        CGContext::add_line_to_point(c, p.x, p.y);
                    }
                    CGContext::set_line_width(c, self.width * 2.);
                    CGContext::set_line_cap(c, CGLineCap::Round);
                    CGContext::set_line_join(c, CGLineJoin::Round);
                    CGContext::replace_path_with_stroked_path(c);
                    CGContext::clip(c);
                }
                draw_stamp(context, &stamp.native, stamp.origin.x, stamp.origin.y);
                CGContext::restore_g_state(c);
                return;
            }
        }
        if matches!(self.tool, Tool::Pen | Tool::Arrow | Tool::Rectangle) {
            self.draw_ink(context, true);
        }
        self.draw_ink(context, false);
    }
    fn draw_ink(&self, context: &CGContext, halo: bool) {
        let Some(first) = self.points.first() else {
            return;
        };
        let Some(last) = self.points.last() else {
            return;
        };
        if !self.width.is_finite()
            || self.width <= 0.0
            || self.color.iter().any(|v| !v.is_finite())
            || self
                .points
                .iter()
                .any(|p| !p.x.is_finite() || !p.y.is_finite())
        {
            return;
        }
        let c = Some(context);
        CGContext::save_g_state(c);
        CGContext::set_allows_antialiasing(c, true);
        CGContext::set_should_antialias(c, true);
        let color = if halo {
            [0.035, 0.04, 0.045, 0.9]
        } else {
            self.color.map(|v| v.clamp(0.0, 1.0))
        };
        CGContext::set_rgb_stroke_color(c, color[0], color[1], color[2], color[3]);
        CGContext::set_rgb_fill_color(c, color[0], color[1], color[2], color[3]);
        CGContext::set_line_cap(c, CGLineCap::Round);
        CGContext::set_line_join(c, CGLineJoin::Round);
        let stroke_width = if halo { self.width * 1.8 } else { self.width };
        CGContext::set_line_width(c, stroke_width);
        if self.tool == Tool::Highlighter {
            CGContext::set_alpha(c, 0.3);
            CGContext::set_line_width(c, self.width * 6.0);
            CGContext::set_line_cap(c, CGLineCap::Butt);
        }
        match self.tool {
            Tool::Pen | Tool::Highlighter => {
                if self.points.iter().all(|point| point == first) {
                    let diameter = stroke_width
                        * if self.tool == Tool::Highlighter {
                            6.0
                        } else {
                            1.0
                        };
                    CGContext::fill_ellipse_in_rect(
                        c,
                        rect(
                            first.x - diameter / 2.0,
                            first.y - diameter / 2.0,
                            diameter,
                            diameter,
                        ),
                    );
                } else {
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, first.x, first.y);
                    for point in &self.points[1..] {
                        CGContext::add_line_to_point(c, point.x, point.y);
                    }
                    CGContext::stroke_path(c);
                }
            }
            Tool::Rectangle => {
                CGContext::stroke_rect(
                    c,
                    rect(
                        first.x.min(last.x),
                        first.y.min(last.y),
                        (last.x - first.x).abs(),
                        (last.y - first.y).abs(),
                    ),
                );
            }
            Tool::Arrow => {
                let dx = last.x - first.x;
                let dy = last.y - first.y;
                let length = dx.hypot(dy);
                if length > 1.0 {
                    let angle = dy.atan2(dx);
                    let head = (self.width * 4.5).max(12.0).min(length * 0.45);
                    let spread = std::f64::consts::PI / 6.0;
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, first.x, first.y);
                    CGContext::add_line_to_point(c, last.x, last.y);
                    CGContext::move_to_point(
                        c,
                        last.x - head * (angle - spread).cos(),
                        last.y - head * (angle - spread).sin(),
                    );
                    CGContext::add_line_to_point(c, last.x, last.y);
                    CGContext::add_line_to_point(
                        c,
                        last.x - head * (angle + spread).cos(),
                        last.y - head * (angle + spread).sin(),
                    );
                    CGContext::stroke_path(c);
                }
            }
            Tool::Text | Tool::PixelateArea | Tool::PixelateBrush => {}
        }
        CGContext::restore_g_state(c);
    }
}
fn draw_stamp(context: &CGContext, image: &Image, x: f64, y: f64) {
    let c = Some(context);
    CGContext::save_g_state(c);
    CGContext::translate_ctm(c, x, y + image.height() as f64);
    CGContext::scale_ctm(c, 1., -1.);
    CGContext::draw_image(
        c,
        rect(0., 0., image.width() as f64, image.height() as f64),
        Some(image.cg_image()),
    );
    CGContext::restore_g_state(c);
}
pub fn from_rgba(width: usize, height: usize, pixels: &[u8]) -> Result<Image, String> {
    let context = bitmap_context(width, height)?;
    if pixels.len() != width * height * 4 {
        return Err("Invalid annotation pixels".into());
    }
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    let data = CGBitmapContextGetData(Some(&context)).cast::<u8>();
    if data.is_null() {
        return Err("Could not access annotation pixels".into());
    }
    for row in 0..height {
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(row * width * 4),
                data.add(row * stride),
                width * 4,
            );
        }
    }
    Image::from_cg(
        CGBitmapContextCreateImage(Some(&context)).ok_or("Could not create annotation image")?,
    )
}
/// Sample only small strips outside the full exported crop. Called by the
/// preparation worker while it still owns the immutable full-screen context.
pub fn redaction_palette(
    image: &Image,
    selection: NSRect,
    bounds: NSRect,
) -> crate::redaction::Palette {
    let Some(crop) = geometry::pixel_rect(
        selection,
        bounds,
        NSSize::new(image.width() as f64, image.height() as f64),
    ) else {
        return crate::redaction::Palette::DARK;
    };
    let mut samples = Vec::new();
    for region in crate::drawing::redaction_sample_regions(image.width(), image.height(), crop) {
        let width = (region.size.width as usize).min(32);
        let height = (region.size.height as usize).min(32);
        if width == 0 || height == 0 {
            continue;
        }
        let Some(strip) = CGImage::with_image_in_rect(Some(image.cg_image()), region) else {
            continue;
        };
        let Ok(context) = bitmap_context(width, height) else {
            continue;
        };
        CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
        CGContext::draw_image(
            Some(&context),
            rect(0., 0., width as f64, height as f64),
            Some(&strip),
        );
        let stride = CGBitmapContextGetBytesPerRow(Some(&context));
        let data = CGBitmapContextGetData(Some(&context)).cast::<u8>();
        if data.is_null() {
            continue;
        }
        for y in 0..height {
            for x in 0..width {
                // The private context owns this storage throughout the read.
                samples.push(unsafe { *data.add(y * stride + x * 4).cast::<[u8; 4]>() });
            }
        }
    }
    crate::redaction::palette_from_samples(&samples)
}
/// AppKit lays out and shapes text on the main thread. Its immutable raster is
/// then safe to composite and encode on workers like every other annotation.
pub fn text_stamp(
    _mtm: objc2::MainThreadMarker,
    text: &str,
    color: [f64; 4],
    scale: f64,
    max_width: f64,
    max_height: f64,
) -> Result<std::sync::Arc<crate::drawing::Stamp>, String> {
    use objc2::runtime::AnyObject;
    use objc2_app_kit::*;
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    if text.trim().is_empty() || text.chars().count() > 4096 {
        return Err("Text must contain 1–4096 characters".into());
    }
    let size = 24. * scale;
    let pad = (size * 0.15).ceil();
    let width = max_width.clamp(size * 3., 4096.);
    let font = NSFont::systemFontOfSize_weight(size, 0.3);
    let ink = NSColor::colorWithSRGBRed_green_blue_alpha(color[0], color[1], color[2], 1.);
    let stroke = NSColor::colorWithSRGBRed_green_blue_alpha(0.035, 0.04, 0.045, 1.);
    let amount = NSNumber::new_f64(-8.);
    let attrs = unsafe {
        NSDictionary::from_slices(
            &[
                NSFontAttributeName,
                NSForegroundColorAttributeName,
                NSStrokeColorAttributeName,
                NSStrokeWidthAttributeName,
            ],
            &[
                &*font as &AnyObject,
                &*stroke as &AnyObject,
                &*stroke as &AnyObject,
                &*amount as &AnyObject,
            ],
        )
    };
    let fill = unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*ink as &AnyObject],
        )
    };
    let text = NSString::from_str(text);
    let measured = unsafe {
        text.boundingRectWithSize_options_attributes_context(
            NSSize::new(width - pad * 2., 4096. - pad * 2.),
            NSStringDrawingOptions::UsesLineFragmentOrigin,
            Some(&attrs),
            None,
        )
    };
    let w = (measured.size.width.ceil() + pad * 2.).min(width).max(1.) as usize;
    let h = (measured.size.height.ceil() + pad * 2.).clamp(1., 4096.) as usize;
    if measured.size.height.ceil() + pad * 2. > max_height || w as f64 > max_width + 1. {
        return Err("Text does not fit here. Shorten it or choose a larger area".into());
    }
    let context = bitmap_context(w, h)?;
    CGContext::translate_ctm(Some(&context), 0., h as f64);
    CGContext::scale_ctm(Some(&context), 1., -1.);
    let graphics = NSGraphicsContext::graphicsContextWithCGContext_flipped(&context, true);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&graphics));
    unsafe {
        text.drawInRect_withAttributes(
            rect(pad, pad, w as f64 - pad * 2., h as f64 - pad * 2.),
            Some(&attrs),
        );
        text.drawInRect_withAttributes(
            rect(pad, pad, w as f64 - pad * 2., h as f64 - pad * 2.),
            Some(&fill),
        );
    }
    NSGraphicsContext::restoreGraphicsState_class();
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    let data = CGBitmapContextGetData(Some(&context)).cast::<u8>();
    if data.is_null() {
        return Err("Could not render annotation text".into());
    }
    let mut rgba = vec![0; w * h * 4];
    for row in 0..h {
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.add(row * stride),
                rgba.as_mut_ptr().add(row * w * 4),
                w * 4,
            );
        }
    }
    // The context already contains premultiplied pixels. Avoid premultiplying twice.
    crate::drawing::Stamp::new(crate::portable_image::Image::from_premultiplied_rgba(
        w, h, rgba,
    )?)
}

/// Dense, owned content for the native annotation diagnostic; no user pixels.
pub fn annotation_fixture(
    mtm: objc2::MainThreadMarker,
    width: usize,
    height: usize,
) -> Result<Image, String> {
    let base = synthetic(width, height)?;
    let mut marks = Vec::new();
    let scale = width as f64 / 1728. * 0.7;
    for (row, label) in [
        "General discussion",
        "Design review",
        "Release planning",
        "Product feedback",
        "Engineering notes",
        "Research updates",
        "Customer requests",
        "Announcements",
        "Weekly planning",
        "Project documents",
        "Ideas and experiments",
        "Support questions",
        "Development",
        "Team resources",
        "Meeting notes",
        "Archive",
    ]
    .into_iter()
    .enumerate()
    {
        for (column, text) in [(0, label), (1, "A private note to hide")] {
            let stamp = text_stamp(
                mtm,
                text,
                [0.95, 0.96, 0.94, 1.],
                scale,
                640. * scale,
                height as f64,
            )?;
            marks.push(Mark {
                tool: Tool::Text,
                points: vec![NSPoint::new(
                    width as f64 * (if column == 0 { 0.17 } else { 0.33 }),
                    height as f64 * 0.18 + row as f64 * height as f64 * 0.035,
                )],
                color: [1.; 4],
                width: 1.,
                stamp: Some(stamp),
            });
        }
    }
    render(&base, &marks)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn bitmap_context(width: usize, height: usize) -> Result<CFRetained<CGContext>, String> {
    validate_dimensions(width, height)?;
    // SAFETY: kCGColorSpaceSRGB is a process-lifetime framework constant.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or("Couldn't create an sRGB color space.")?;
    // SAFETY: Null data delegates allocation/lifetime to CoreGraphics. Dimensions
    // were checked above, and 8-bit premultiplied RGBA is supported by sRGB.
    unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
        )
    }
    .ok_or_else(|| "Couldn't allocate the screenshot image.".into())
}

pub fn crop(image: &Image, selection: NSRect, bounds: NSRect) -> Result<Image, String> {
    let pixels = geometry::pixel_rect(
        selection,
        bounds,
        NSSize::new(image.width() as f64, image.height() as f64),
    )
    .ok_or("Select an area at least two points wide and high.")?;
    let cropped = CGImage::with_image_in_rect(Some(image.cg_image()), pixels)
        .ok_or("Couldn't crop the selected area.")?;
    Image::from_cg(cropped)
}

/// Return the existing immutable image for an unmarked copy; allocate only when
/// annotations actually need compositing.
pub fn render(image: &Image, marks: &[Mark]) -> Result<Image, String> {
    if marks.is_empty() {
        return Ok(image.clone());
    }
    let context = bitmap_context(image.width(), image.height())?;
    let c = Some(&*context);
    CGContext::draw_image(
        c,
        rect(0.0, 0.0, image.width() as f64, image.height() as f64),
        Some(image.cg_image()),
    );
    CGContext::translate_ctm(c, 0.0, image.height() as f64);
    CGContext::scale_ctm(c, 1.0, -1.0);
    for mark in marks {
        mark.draw(&context);
    }
    Image::from_cg(CGBitmapContextCreateImage(c).ok_or("Couldn't render the annotations.")?)
}

#[derive(Debug)]
pub struct EncodedImage {
    pub png: Vec<u8>,
    pub tiff: Vec<u8>,
}

fn encode_type(image: &Image, identifier: &str) -> Result<Vec<u8>, String> {
    let data = CFMutableData::new(None, 0).ok_or("Couldn't allocate encoded image data.")?;
    let identifier = CFString::from_str(identifier);
    // SAFETY: The options/properties dictionaries are absent. Both destination
    // and backing data live through finalization, with no concurrent mutation.
    let destination = unsafe { CGImageDestination::with_data(&data, &identifier, 1, None) }
        .ok_or("Couldn't create the image encoder.")?;
    unsafe {
        destination.add_image(image.cg_image(), None);
        if !destination.finalize() {
            return Err("Couldn't finish encoding the screenshot.".into());
        }
    }
    Ok(data.to_vec())
}

pub fn encode(image: &Image) -> Result<EncodedImage, String> {
    Ok(EncodedImage {
        png: encode_png(image)?,
        tiff: encode_tiff(image)?,
    })
}

pub fn encode_png(image: &Image) -> Result<Vec<u8>, String> {
    // Normalizing through a private sRGB RGBA bitmap handles ScreenCaptureKit's
    // BGRA output, cropped images, arbitrary source stride and color profiles.
    // No assumption about the source image's data provider layout is required.
    let width = image.width();
    let height = image.height();
    let context = bitmap_context(width, height)?;
    CGContext::draw_image(
        Some(&context),
        rect(0.0, 0.0, width as f64, height as f64),
        Some(image.cg_image()),
    );
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    let row_bytes = width.checked_mul(4).ok_or("PNG row size overflow.")?;
    let allocation = stride
        .checked_mul(height)
        .ok_or("PNG image size overflow.")?;
    if stride < row_bytes || allocation > 514 * 1024 * 1024 {
        return Err("The PNG bitmap layout is unsupported.".into());
    }
    let pointer = CGBitmapContextGetData(Some(&context)).cast::<u8>();
    if pointer.is_null() {
        return Err("Couldn't access the PNG image pixels.".into());
    }
    // SAFETY: This private context owns allocation writable bytes, checked
    // above. No CGImage aliases it, no drawing runs concurrently, and context
    // outlives the slice. Explicit big-endian premultiplied-last means RGBA.
    let pixels = unsafe { std::slice::from_raw_parts_mut(pointer, allocation) };
    for row in 0..height {
        let source = row * stride;
        let target = row * row_bytes;
        if source != target {
            pixels.copy_within(source..source + row_bytes, target);
        }
        for pixel in pixels[target..target + row_bytes].as_chunks_mut::<4>().0 {
            let alpha = u32::from(pixel[3]);
            if alpha == 255 {
                continue;
            }
            // PNG stores straight alpha; Quartz stores premultiplied alpha.
            // Round to nearest while preserving the premultiplied round trip.
            // A fully transparent pixel has canonical zero color channels.
            for channel in &mut pixel[..3] {
                *channel = (u32::from(*channel) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8;
            }
        }
    }
    let packed_bytes = row_bytes * height;
    let mut output = Vec::new();
    output
        .try_reserve((packed_bytes / 8).clamp(1024, 4 * 1024 * 1024))
        .map_err(|_| "Couldn't allocate the PNG output buffer.")?;
    {
        let mut encoder = png::Encoder::new(&mut output, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("Couldn't start PNG encoding: {error}"))?;
        writer
            .write_image_data(&pixels[..packed_bytes])
            .map_err(|error| format!("Couldn't encode the screenshot: {error}"))?;
        writer
            .finish()
            .map_err(|error| format!("Couldn't finish PNG encoding: {error}"))?;
    }
    Ok(output)
}

pub fn encode_tiff(image: &Image) -> Result<Vec<u8>, String> {
    encode_type(image, "public.tiff")
}

/// Deterministic desktop for interaction testing, without screen permissions.
pub fn synthetic(width: usize, height: usize) -> Result<Image, String> {
    let context = bitmap_context(width, height)?;
    let c = Some(&*context);
    let w = width as f64;
    let h = height as f64;
    for row in 0..height {
        let t = row as f64 / h;
        CGContext::set_rgb_fill_color(c, 0.10 + 0.08 * t, 0.15 + 0.10 * t, 0.15 + 0.05 * t, 1.0);
        CGContext::fill_rect(c, rect(0.0, row as f64, w, 1.0));
    }
    CGContext::set_rgb_fill_color(c, 0.40, 0.48, 0.31, 0.5);
    CGContext::fill_ellipse_in_rect(c, rect(w * 0.50, h * 0.25, w * 0.6, w * 0.6));
    CGContext::set_rgb_fill_color(c, 0.75, 0.61, 0.43, 0.35);
    CGContext::fill_ellipse_in_rect(c, rect(-w * 0.2, -h * 0.65, w * 0.9, w * 0.9));
    CGContext::set_rgb_fill_color(c, 0.93, 0.93, 0.88, 1.0);
    CGContext::fill_rect(c, rect(w * 0.18, h * 0.20, w * 0.61, h * 0.59));
    CGContext::set_rgb_fill_color(c, 0.86, 0.87, 0.81, 1.0);
    CGContext::fill_rect(c, rect(w * 0.18, h * 0.73, w * 0.61, h * 0.06));
    for (index, color) in [[0.96, 0.39, 0.35], [0.96, 0.75, 0.32], [0.41, 0.76, 0.46]]
        .into_iter()
        .enumerate()
    {
        CGContext::set_rgb_fill_color(c, color[0], color[1], color[2], 1.0);
        CGContext::fill_ellipse_in_rect(
            c,
            rect(
                w * (0.20 + index as f64 * 0.019),
                h * 0.749,
                h * 0.015,
                h * 0.015,
            ),
        );
    }
    CGContext::set_rgb_fill_color(c, 0.82, 0.84, 0.75, 1.0);
    CGContext::fill_rect(c, rect(w * 0.20, h * 0.23, w * 0.13, h * 0.47));
    for index in 0..6 {
        CGContext::set_rgb_fill_color(c, 0.58, 0.64, 0.50, 0.55);
        CGContext::fill_rect(
            c,
            rect(
                w * 0.216,
                h * (0.65 - index as f64 * 0.06),
                w * 0.096,
                h * 0.012,
            ),
        );
    }
    for index in 0..5 {
        CGContext::set_rgb_fill_color(c, 0.29, 0.36, 0.25, if index == 0 { 0.8 } else { 0.20 });
        CGContext::fill_rect(
            c,
            rect(
                w * 0.365,
                h * (0.64 - index as f64 * 0.07),
                w * if index == 0 { 0.25 } else { 0.36 },
                h * if index == 0 { 0.026 } else { 0.014 },
            ),
        );
    }
    CGContext::set_rgb_fill_color(c, 0.76, 0.85, 0.56, 1.0);
    CGContext::fill_rect(c, rect(w * 0.365, h * 0.26, w * 0.19, h * 0.07));
    Image::from_cg(CGBitmapContextCreateImage(c).ok_or("Couldn't create the demonstration image.")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFData;
    use objc2_core_graphics::CGBitmapContextGetData;
    use objc2_image_io::CGImageSource;
    #[test]
    fn prepared_redaction_and_patch_keep_native_orientation() {
        let source = crate::portable_image::synthetic(96, 80).unwrap();
        let native = from_rgba(96, 80, source.pixels()).unwrap();
        let mut e = crate::editor::Editor::default();
        let (revision, seed, _) = e.pixelation_job().unwrap();
        e.complete_pixelation(
            revision,
            Some(
                crate::drawing::Stamp::new(
                    crate::redaction::generate(96, 80, 8, seed, crate::redaction::Palette::DARK)
                        .unwrap(),
                )
                .unwrap(),
            ),
        );
        e.tool = Tool::PixelateArea;
        e.begin(NSPoint::new(16., 12.), 1.);
        e.end(NSPoint::new(44., 36.));
        let expected = crate::portable_image::render(&source, &e.marks).unwrap();
        let actual = render(&native, &e.marks).unwrap();
        for (x, y) in [(18, 14), (30, 25), (40, 32), (2, 2), (90, 70)] {
            assert_eq!(
                sample(&actual, x, y),
                expected.pixels()[(y * 96 + x) * 4..(y * 96 + x) * 4 + 4]
            );
        }
    }

    #[test]
    fn automatic_redaction_palette_is_independent_of_the_entire_retina_crop() {
        let bounds = rect(7., 11., 40., 30.);
        let selection = rect(12.125, 15.375, 18.2, 9.4);
        let exported = geometry::pixel_rect(selection, bounds, NSSize::new(80., 60.)).unwrap();
        let make = |hidden: [u8; 4]| {
            let mut rgba = [214, 211, 205, 255].repeat(80 * 60);
            for y in exported.origin.y as usize..(exported.origin.y + exported.size.height) as usize
            {
                for x in
                    exported.origin.x as usize..(exported.origin.x + exported.size.width) as usize
                {
                    rgba[(y * 80 + x) * 4..(y * 80 + x) * 4 + 4].copy_from_slice(&hidden);
                }
            }
            from_rgba(80, 60, &rgba).unwrap()
        };
        let a = make([255, 255, 255, 255]);
        let b = make([0, 0, 0, 255]);
        let palette = redaction_palette(&a, selection, bounds);
        let other_palette = redaction_palette(&b, selection, bounds);
        assert_eq!(palette, other_palette);
        assert_eq!(
            palette,
            crate::redaction::palette_from_samples(&[[214, 211, 205, 255]])
        );
        let texture = |palette| {
            crate::redaction::generate(
                exported.size.width as usize,
                exported.size.height as usize,
                8,
                [19; 32],
                palette,
            )
            .unwrap()
        };
        assert_eq!(texture(palette).pixels(), texture(other_palette).pixels());
        assert_eq!(
            redaction_palette(&a, bounds, bounds),
            crate::redaction::Palette::DARK
        );
    }

    fn png_rgba(image: &Image) -> Vec<u8> {
        let bytes = encode_png(image).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut rgba).unwrap();
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        rgba.truncate(frame.buffer_size());
        rgba
    }

    #[test]
    fn native_redaction_masks_replace_every_covered_pixel_without_source_blending() {
        let a = [219, 16, 122, 255];
        let b = [1, 244, 185, 255];
        let source_a = from_rgba(32, 24, &a.repeat(32 * 24)).unwrap();
        let source_b = from_rgba(32, 24, &b.repeat(32 * 24)).unwrap();
        let stamp = crate::drawing::Stamp::new(
            crate::redaction::generate(32, 24, 4, [71; 32], crate::redaction::Palette::DARK)
                .unwrap(),
        )
        .unwrap();
        for (tool, points, width) in [
            (
                Tool::PixelateArea,
                vec![NSPoint::new(4.2, 5.7), NSPoint::new(25.8, 19.1)],
                1.,
            ),
            (Tool::PixelateBrush, vec![NSPoint::new(15.4, 11.7)], 4.2),
            (
                Tool::PixelateBrush,
                vec![
                    NSPoint::new(4.4, 5.2),
                    NSPoint::new(25.8, 18.6),
                    NSPoint::new(18.4, 3.2),
                ],
                3.2,
            ),
        ] {
            let mark = Mark {
                tool,
                points,
                width,
                color: [1.; 4],
                stamp: Some(stamp.clone()),
            };
            let pixels_a = png_rgba(&render(&source_a, std::slice::from_ref(&mark)).unwrap());
            let pixels_b = png_rgba(&render(&source_b, std::slice::from_ref(&mark)).unwrap());
            let mut covered = 0;
            for (index, (pa, pb)) in pixels_a
                .as_chunks::<4>()
                .0
                .iter()
                .zip(pixels_b.as_chunks::<4>().0)
                .enumerate()
            {
                if pa == pb {
                    covered += 1;
                    assert_eq!(pa[3], 255);
                } else {
                    assert_eq!(*pa, a, "partial source mix at {index}, {tool:?}");
                    assert_eq!(*pb, b, "partial source mix at {index}, {tool:?}");
                }
            }
            assert!(covered > 40);
            if tool == Tool::PixelateArea {
                for y in 5..20 {
                    for x in 4..26 {
                        let offset = (y * 32 + x) * 4;
                        assert_eq!(pixels_a[offset..offset + 4], pixels_b[offset..offset + 4]);
                    }
                }
            }
            // Cropping the cached stamp at commit preserves the same coverage.
            let mut patched = mark.clone();
            patched.stamp = Some(stamp.patch(&mark).unwrap());
            assert_eq!(pixels_a, png_rgba(&render(&source_a, &[patched]).unwrap()));
        }
    }

    fn pattern() -> Image {
        let context = bitmap_context(100, 80).unwrap();
        let c = Some(&*context);
        CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(c, rect(0.0, 0.0, 100.0, 80.0));
        CGContext::set_rgb_fill_color(c, 1.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(c, rect(0.0, 40.0, 50.0, 40.0));
        CGContext::set_rgb_fill_color(c, 0.0, 0.0, 1.0, 1.0);
        CGContext::fill_rect(c, rect(50.0, 0.0, 50.0, 40.0));
        Image::from_cg(CGBitmapContextCreateImage(c).unwrap()).unwrap()
    }

    fn sample(image: &Image, x: usize, y: usize) -> [u8; 4] {
        let pixel =
            CGImage::with_image_in_rect(Some(image.cg_image()), rect(x as f64, y as f64, 1.0, 1.0))
                .unwrap();
        let context = bitmap_context(1, 1).unwrap();
        CGContext::draw_image(Some(&context), rect(0.0, 0.0, 1.0, 1.0), Some(&pixel));
        // SAFETY: This live 1×1 RGBA bitmap owns at least four initialized bytes.
        let data = unsafe {
            std::slice::from_raw_parts(CGBitmapContextGetData(Some(&context)).cast::<u8>(), 4)
        };
        [data[0], data[1], data[2], data[3]]
    }

    #[test]
    fn image_is_send_and_sync() {
        fn assert_traits<T: Send + Sync>() {}
        assert_traits::<Image>();
    }

    #[test]
    fn dimensions_reject_zero_overflow_and_excessive_allocation() {
        for (w, h) in [(0, 10), (10, 0), (usize::MAX, 2), (32000, 32000)] {
            assert!(validate_dimensions(w, h).is_err());
        }
        assert!(validate_dimensions(6016, 3384).is_ok());
    }

    #[test]
    fn cropping_keeps_top_left_orientation_and_original_resolution() {
        let source = pattern();
        let top = crop(
            &source,
            rect(0.0, 0.0, 25.0, 20.0),
            rect(0.0, 0.0, 50.0, 40.0),
        )
        .unwrap();
        let bottom = crop(
            &source,
            rect(25.0, 20.0, 25.0, 20.0),
            rect(0.0, 0.0, 50.0, 40.0),
        )
        .unwrap();
        assert_eq!((top.width(), top.height()), (50, 40));
        assert_eq!(sample(&top, 10, 10), [255, 0, 0, 255]);
        assert_eq!(sample(&bottom, 10, 10), [0, 0, 255, 255]);
    }

    #[test]
    fn unmarked_render_keeps_the_same_image_allocation() {
        let source = pattern();
        let result = render(&source, &[]).unwrap();
        assert!(std::ptr::eq(source.cg_image(), result.cg_image()));
    }

    #[test]
    fn annotations_preserve_source_and_use_top_left_coordinates() {
        let source = pattern();
        let mark = Mark {
            tool: Tool::Pen,
            points: vec![NSPoint::new(15.0, 10.0)],
            color: [0.0, 1.0, 0.0, 1.0],
            width: 10.0,
            stamp: None,
        };
        let result = render(&source, &[mark]).unwrap();
        assert_eq!(sample(&result, 15, 10), [0, 255, 0, 255]);
        assert_eq!(sample(&result, 15, 70), [255, 255, 255, 255]);
        assert_eq!(sample(&source, 15, 10), [255, 0, 0, 255]);
    }

    #[test]
    fn highlighter_composites_at_thirty_percent_opacity() {
        let source = pattern();
        let mark = Mark {
            tool: Tool::Highlighter,
            points: vec![NSPoint::new(20.0, 60.0), NSPoint::new(40.0, 60.0)],
            color: [1.0, 0.0, 0.0, 1.0],
            width: 2.0,
            stamp: None,
        };
        let result = render(&source, &[mark]).unwrap();
        let pixel = sample(&result, 30, 60);
        assert_eq!(pixel[0], 255);
        assert!((pixel[1] as i16 - 179).abs() <= 2, "{pixel:?}");
        assert!((pixel[2] as i16 - 179).abs() <= 2, "{pixel:?}");
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn all_tools_render_visible_pixels() {
        let source = pattern();
        for (tool, point) in [
            (Tool::Pen, (30, 60)),
            (Tool::Arrow, (30, 60)),
            (Tool::Rectangle, (30, 50)),
            (Tool::Highlighter, (30, 60)),
        ] {
            let points = if tool == Tool::Rectangle {
                vec![NSPoint::new(20.0, 50.0), NSPoint::new(40.0, 70.0)]
            } else {
                vec![NSPoint::new(20.0, 60.0), NSPoint::new(40.0, 60.0)]
            };
            let mark = Mark {
                tool,
                points,
                color: [0.0, 0.0, 0.0, 1.0],
                width: 3.0,
                stamp: None,
            };
            let result = render(&source, &[mark]).unwrap();
            assert!(sample(&result, point.0, point.1)[0] < 220, "{tool:?}");
        }
    }

    #[test]
    fn png_and_tiff_round_trip_dimensions_colors_and_orientation() {
        let source = pattern();
        let encoded = encode(&source).unwrap();
        assert_eq!(&encoded.png[..8], b"\x89PNG\r\n\x1a\n");
        for bytes in [&encoded.png, &encoded.tiff] {
            let data = CFData::from_bytes(bytes);
            // SAFETY: No option dictionary is passed; data remains alive.
            let decoder = unsafe { CGImageSource::with_data(&data, None) }.unwrap();
            let image =
                Image::from_cg(unsafe { decoder.image_at_index(0, None) }.unwrap()).unwrap();
            assert_eq!((image.width(), image.height()), (100, 80));
            assert_eq!(sample(&image, 10, 10), [255, 0, 0, 255]);
            assert_eq!(sample(&image, 70, 60), [0, 0, 255, 255]);
        }
    }

    #[test]
    fn encoders_preserve_every_opaque_rgb_pixel() {
        let context = bitmap_context(32, 32).unwrap();
        let c = Some(&*context);
        for y in 0..32 {
            for x in 0..32 {
                CGContext::set_rgb_fill_color(
                    c,
                    (x * 8 + y % 8) as f64 / 255.0,
                    (y * 8 + x % 8) as f64 / 255.0,
                    ((x * 17 + y * 23) % 256) as f64 / 255.0,
                    1.0,
                );
                CGContext::fill_rect(c, rect(x as f64, y as f64, 1.0, 1.0));
            }
        }
        let source = Image::from_cg(CGBitmapContextCreateImage(c).unwrap()).unwrap();
        let encoded = encode(&source).unwrap();
        for bytes in [&encoded.png, &encoded.tiff] {
            let data = CFData::from_bytes(bytes);
            // SAFETY: No option dictionary is passed; data remains alive.
            let decoder = unsafe { CGImageSource::with_data(&data, None) }.unwrap();
            let decoded =
                Image::from_cg(unsafe { decoder.image_at_index(0, None) }.unwrap()).unwrap();
            for y in 0..32 {
                for x in 0..32 {
                    assert_eq!(
                        sample(&decoded, x, y),
                        sample(&source, x, y),
                        "pixel {x},{y}"
                    );
                }
            }
        }
    }

    fn decode(bytes: &[u8]) -> Image {
        let data = CFData::from_bytes(bytes);
        // SAFETY: No option dictionary is passed and the input data is retained.
        let decoder = unsafe { CGImageSource::with_data(&data, None) }.unwrap();
        Image::from_cg(unsafe { decoder.image_at_index(0, None) }.unwrap()).unwrap()
    }

    #[test]
    fn png_unpremultiplies_every_alpha_level_without_changing_visible_color() {
        let context = bitmap_context(256, 3).unwrap();
        let stride = CGBitmapContextGetBytesPerRow(Some(&context));
        // SAFETY: Private context owns three rows of stride bytes; no image yet
        // aliases the buffer. All written channels are <= alpha (valid RGBA).
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(
                CGBitmapContextGetData(Some(&context)).cast::<u8>(),
                stride * 3,
            )
        };
        for y in 0..3 {
            for alpha in 0..256 {
                let p = &mut pixels[y * stride + alpha * 4..][..4];
                p.copy_from_slice(&[
                    (alpha / (y + 1)) as u8,
                    (alpha / 3) as u8,
                    (alpha / 2) as u8,
                    alpha as u8,
                ]);
            }
        }
        let source = Image::from_cg(CGBitmapContextCreateImage(Some(&context)).unwrap()).unwrap();
        let decoded = decode(&encode_png(&source).unwrap());
        for y in 0..3 {
            for x in 0..256 {
                assert_eq!(
                    sample(&decoded, x, y),
                    sample(&source, x, y),
                    "alpha {x}, row {y}"
                );
            }
        }
    }

    #[test]
    fn png_normalizes_padded_bgra_crops_and_preserves_orientation() {
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
        // SAFETY: Null storage is allocated by Quartz; 32-byte stride exceeds
        // the required 20 bytes, and the requested BGRA format is supported.
        let context = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                5,
                4,
                8,
                32,
                Some(&space),
                CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
            )
        }
        .unwrap();
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(
                CGBitmapContextGetData(Some(&context)).cast::<u8>(),
                32 * 4,
            )
        };
        for y in 0..4 {
            for x in 0..5 {
                let p = &mut pixels[y * 32 + x * 4..][..4];
                p.copy_from_slice(&[(x * 51) as u8, (y * 70) as u8, 131, 255]);
            }
        }
        let source = Image::from_cg(CGBitmapContextCreateImage(Some(&context)).unwrap()).unwrap();
        let source = crop(&source, rect(1.0, 1.0, 3.0, 3.0), rect(0.0, 0.0, 5.0, 4.0)).unwrap();
        let decoded = decode(&encode_png(&source).unwrap());
        assert_eq!((decoded.width(), decoded.height()), (3, 3));
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(
                    sample(&decoded, x, y),
                    sample(&source, x, y),
                    "pixel {x},{y}"
                );
            }
        }
    }

    #[test]
    fn png_converts_wide_gamut_source_to_tagged_srgb() {
        let space =
            CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceDisplayP3 }))
                .unwrap();
        // SAFETY: Null allocation and supported 8-bit RGBA in a valid RGB space.
        let context = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                3,
                3,
                8,
                0,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }
        .unwrap();
        CGContext::set_rgb_fill_color(Some(&context), 0.9, 0.3, 0.1, 1.0);
        CGContext::fill_rect(Some(&context), rect(0.0, 0.0, 3.0, 3.0));
        let source = Image::from_cg(CGBitmapContextCreateImage(Some(&context)).unwrap()).unwrap();
        let bytes = encode_png(&source).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        assert_eq!(
            decoder.info().srgb,
            Some(png::SrgbRenderingIntent::Perceptual)
        );
        let decoded = decode(&bytes);
        assert_eq!(sample(&decoded, 1, 1), sample(&source, 1, 1));
    }
}
