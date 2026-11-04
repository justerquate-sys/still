//! Pure-Rust Windows image backend, also tested on macOS. Pixels are immutable
//! premultiplied RGBA; PNG and DIBV5 exports use original crop dimensions.
use crate::{
    coordinates::{Rect, Size},
    drawing::{Mark, Tool},
    geometry,
};
use std::sync::Arc;
use tiny_skia::{
    FillRule, IntSize, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapMut, Stroke, Transform,
};
#[derive(Clone, Debug)]
pub struct Image(Arc<PixelData>);
#[derive(Debug)]
struct PixelData {
    rgba: Pixmap,
    bgra: Vec<u8>,
}
impl Image {
    pub fn from_premultiplied_rgba(
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    ) -> Result<Self, String> {
        validate_dimensions(width, height)?;
        if rgba.len() != width * height * 4 {
            return Err("Invalid annotation pixels".into());
        }
        Pixmap::from_vec(
            rgba,
            IntSize::from_wh(width as u32, height as u32).ok_or("Invalid annotation size")?,
        )
        .map(Self::from_pixmap)
        .ok_or("Invalid annotation pixels".into())
    }
    fn from_pixmap(rgba: Pixmap) -> Self {
        // Prepare the native upload format when the image is created, normally
        // on a worker. Pointer frames never convert or copy the full screenshot.
        let mut bgra = rgba.data().to_vec();
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Self(Arc::new(PixelData { rgba, bgra }))
    }
    pub fn from_rgba(width: usize, height: usize, mut rgba: Vec<u8>) -> Result<Self, String> {
        validate_dimensions(width, height)?;
        if rgba.len() != width * height * 4 {
            return Err("Invalid screenshot pixel buffer".into());
        }
        for p in rgba.as_chunks_mut::<4>().0 {
            let a = p[3] as u16;
            for c in &mut p[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
        Pixmap::from_vec(
            rgba,
            IntSize::from_wh(width as u32, height as u32).ok_or("Invalid image size")?,
        )
        .map(Self::from_pixmap)
        .ok_or_else(|| "Could not allocate image".into())
    }
    pub fn width(&self) -> usize {
        self.0.rgba.width() as usize
    }
    pub fn height(&self) -> usize {
        self.0.rgba.height() as usize
    }
    pub fn pixels(&self) -> &[u8] {
        self.0.rgba.data()
    }
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
    pub fn bgra(&self) -> &[u8] {
        &self.0.bgra
    }
}
pub fn validate_dimensions(width: usize, height: usize) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > 32768
        || height > 32768
        || width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .is_none_or(|n| n > 512 * 1024 * 1024)
    {
        return Err("Screenshot is too large (maximum 128 megapixels)".into());
    }
    Ok(())
}
pub fn crop(image: &Image, selection: Rect, bounds: Rect) -> Result<Image, String> {
    let r = geometry::pixel_rect(
        selection,
        bounds,
        Size::new(image.width() as f64, image.height() as f64),
    )
    .ok_or("Select an area at least two pixels wide and high")?;
    let (x, y, w, h) = (
        r.origin.x as usize,
        r.origin.y as usize,
        r.size.width as usize,
        r.size.height as usize,
    );
    let mut data = vec![0; w * h * 4];
    for row in 0..h {
        data[row * w * 4..(row + 1) * w * 4].copy_from_slice(
            &image.pixels()
                [((y + row) * image.width() + x) * 4..((y + row) * image.width() + x + w) * 4],
        );
    }
    let pix = Pixmap::from_vec(
        data,
        IntSize::from_wh(w as u32, h as u32).ok_or("Invalid crop")?,
    )
    .ok_or("Invalid crop pixels")?;
    Ok(Image::from_pixmap(pix))
}
pub fn render(image: &Image, marks: &[Mark]) -> Result<Image, String> {
    if marks.is_empty() {
        return Ok(image.clone());
    }
    let mut pix = image.0.rgba.clone();
    for mark in marks {
        draw(&mut pix.as_mut(), mark);
    }
    Ok(Image::from_pixmap(pix))
}
fn draw(pix: &mut PixmapMut<'_>, mark: &Mark) {
    if matches!(mark.tool, Tool::Pen | Tool::Arrow | Tool::Rectangle) {
        draw_ink(pix, mark, true);
    }
    draw_ink(pix, mark, false);
}
fn draw_ink(pix: &mut PixmapMut<'_>, mark: &Mark, halo: bool) {
    if !mark.valid() {
        return;
    }
    let first = mark.points[0];
    let last = *mark.points.last().unwrap();
    if mark.tool == Tool::Text {
        if let Some(stamp) = &mark.stamp {
            pix.draw_pixmap(
                first.x.round() as i32,
                first.y.round() as i32,
                stamp.pixels.0.rgba.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        return;
    }
    if mark.tool.is_pixelation() {
        let Some(stamp) = &mark.stamp else {
            return;
        };
        let mut path = PathBuilder::new();
        if mark.tool == Tool::PixelateArea {
            if let Some(rect) = tiny_skia::Rect::from_ltrb(
                first.x.min(last.x) as f32,
                first.y.min(last.y) as f32,
                first.x.max(last.x) as f32,
                first.y.max(last.y) as f32,
            ) {
                path.push_rect(rect);
            }
        } else if mark.points.iter().all(|p| p == &first) {
            if let Some(rect) = tiny_skia::Rect::from_xywh(
                (first.x - mark.width) as f32,
                (first.y - mark.width) as f32,
                (mark.width * 2.) as f32,
                (mark.width * 2.) as f32,
            ) {
                path.push_oval(rect);
            }
        } else {
            path.move_to(first.x as f32, first.y as f32);
            for p in &mark.points[1..] {
                path.line_to(p.x as f32, p.y as f32);
            }
        }
        if let Some(mut path) = path.finish() {
            if mark.tool == Tool::PixelateBrush && mark.points.iter().any(|p| p != &first) {
                let Some(stroked) = path.stroke(
                    &Stroke {
                        width: (mark.width * 2.) as f32,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    },
                    1.,
                ) else {
                    return;
                };
                path = stroked;
            }
            let mut mask = tiny_skia::Mask::new(pix.width(), pix.height())
                .expect("validated image dimensions");
            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
            pix.draw_pixmap(
                stamp.origin.x.round() as i32,
                stamp.origin.y.round() as i32,
                stamp.pixels.0.rgba.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                Transform::identity(),
                Some(&mask),
            );
        }
        return;
    }
    let high = mark.tool == Tool::Highlighter;
    let mut paint = Paint::default();
    let c = if halo {
        [0.035, 0.04, 0.045, 0.9]
    } else {
        mark.color.map(|c| c.clamp(0., 1.) as f32)
    };
    paint.set_color_rgba8(
        (c[0] * 255.).round() as u8,
        (c[1] * 255.).round() as u8,
        (c[2] * 255.).round() as u8,
        (c[3] * if high { 76.5 } else { 255. }).round() as u8,
    );
    paint.anti_alias = true;
    let width = mark.width as f32
        * if high {
            6.
        } else if halo {
            1.8
        } else {
            1.
        };
    let mut path = PathBuilder::new();
    if matches!(mark.tool, Tool::Pen | Tool::Highlighter) && mark.points.iter().all(|p| p == &first)
    {
        if let Some(r) = tiny_skia::Rect::from_xywh(
            first.x as f32 - width / 2.,
            first.y as f32 - width / 2.,
            width,
            width,
        ) {
            path.push_oval(r);
            if let Some(path) = path.finish() {
                pix.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        return;
    }
    path.move_to(first.x as f32, first.y as f32);
    match mark.tool {
        Tool::Rectangle => {
            path.line_to(last.x as f32, first.y as f32);
            path.line_to(last.x as f32, last.y as f32);
            path.line_to(first.x as f32, last.y as f32);
            path.close();
        }
        Tool::Arrow => {
            let Some(head) = mark.arrow_head() else {
                return;
            };
            path.line_to(last.x as f32, last.y as f32);
            path.move_to(head[0].x as f32, head[0].y as f32);
            path.line_to(head[1].x as f32, head[1].y as f32);
            path.line_to(head[2].x as f32, head[2].y as f32);
        }
        _ => {
            for p in &mark.points[1..] {
                path.line_to(p.x as f32, p.y as f32);
            }
        }
    }
    if let Some(path) = path.finish() {
        pix.stroke_path(
            &path,
            &paint,
            &Stroke {
                width,
                line_cap: if high { LineCap::Butt } else { LineCap::Round },
                line_join: LineJoin::Round,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}
/// A fixed grid sampled once from the frozen crop. Every mask uses these same
/// pixels, so strokes do not shimmer or change color as the pointer moves.
pub fn pixelated(
    image: &Image,
    cell: usize,
) -> Result<std::sync::Arc<crate::drawing::Stamp>, String> {
    let cell = cell.clamp(4, 128);
    let mut pix = image.0.rgba.clone();
    let w = image.width();
    let h = image.height();
    for y in (0..h).step_by(cell) {
        for x in (0..w).step_by(cell) {
            let right = (x + cell).min(w);
            let bottom = (y + cell).min(h);
            let count = ((right - x) * (bottom - y)) as u64;
            let mut sum = [0u64; 4];
            for yy in y..bottom {
                for xx in x..right {
                    let i = (yy * w + xx) * 4;
                    for (c, s) in sum.iter_mut().enumerate() {
                        *s += image.pixels()[i + c] as u64;
                    }
                }
            }
            let avg = sum.map(|s| ((s + count / 2) / count) as u8);
            for yy in y..bottom {
                for xx in x..right {
                    let i = (yy * w + xx) * 4;
                    pix.data_mut()[i..i + 4].copy_from_slice(&avg);
                }
            }
        }
    }
    crate::drawing::Stamp::new(Image::from_pixmap(pix))
}
pub struct EncodedImage {
    pub png: Vec<u8>,
    pub dibv5: Vec<u8>,
}
pub fn encode(image: &Image) -> Result<EncodedImage, String> {
    let mut rgba = image.pixels().to_vec();
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder =
            png::Encoder::new(&mut bytes, image.width() as u32, image.height() as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.set_compression(png::Compression::Fast);
        encoder
            .write_header()
            .and_then(|mut w| w.write_image_data(&rgba))
            .map_err(|e| e.to_string())?;
    }
    let mut dib = vec![0u8; 124 + rgba.len()];
    for (offset, value) in [
        (0, 124u32),
        (4, image.width() as u32),
        (8, (-(image.height() as i32)) as u32),
        (16, 3),
        (20, rgba.len() as u32),
        (40, 0x00ff0000),
        (44, 0x0000ff00),
        (48, 0x000000ff),
        (52, 0xff000000),
        (56, 0x73524742),
        (108, 4),
    ] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    for p in rgba.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    dib[124..].copy_from_slice(&rgba);
    Ok(EncodedImage {
        png: bytes,
        dibv5: dib,
    })
}
pub fn synthetic(width: usize, height: usize) -> Result<Image, String> {
    validate_dimensions(width, height)?;
    let mut data = vec![0; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            data[i..i + 4].copy_from_slice(&[
                ((x * 255 / width) as u8),
                ((y * 255 / height) as u8),
                96,
                255,
            ]);
        }
    }
    Image::from_rgba(width, height, data)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinates::Point;
    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(Point::new(x, y), Size::new(w, h))
    }
    #[test]
    fn mosaic_masks_keep_unselected_pixels_and_original_source() {
        let source = synthetic(64, 48).unwrap();
        let original = source.pixels().to_vec();
        let mosaic = pixelated(&source, 8).unwrap();
        let at = |image: &Image, x: usize, y: usize| {
            image.pixels()[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4].to_vec()
        };
        assert_eq!(at(&mosaic.pixels, 9, 9), at(&mosaic.pixels, 14, 14));
        let mut area = Mark {
            tool: Tool::PixelateArea,
            points: vec![Point::new(10., 10.), Point::new(30., 24.)],
            color: [1.; 4],
            width: 5.,
            stamp: Some(mosaic.clone()),
        };
        let result = render(&source, &[area.clone()]).unwrap();
        assert_eq!(at(&result, 12, 12), at(&mosaic.pixels, 12, 12));
        assert_eq!(at(&result, 8, 12), at(&source, 8, 12));
        area.tool = Tool::PixelateBrush;
        area.points = vec![Point::new(8., 32.), Point::new(52., 32.)];
        let result = render(&source, &[area]).unwrap();
        assert_eq!(at(&result, 25, 32), at(&mosaic.pixels, 25, 32));
        assert_eq!(at(&result, 25, 20), at(&source, 25, 20));
        assert_eq!(source.pixels(), original);
    }
    #[test]
    fn prepared_text_stamp_keeps_orientation_and_history() {
        let source = synthetic(32, 24).unwrap();
        let mut pixels = vec![0; 6 * 4 * 4];
        for y in 0..4 {
            for x in 0..6 {
                let i = (y * 6 + x) * 4;
                pixels[i..i + 4].copy_from_slice(&[x as u8 * 30, y as u8 * 60, 255, 255]);
            }
        }
        let stamp =
            crate::drawing::Stamp::new(Image::from_rgba(6, 4, pixels.clone()).unwrap()).unwrap();
        let mut editor = crate::editor::Editor::default();
        assert!(editor.insert_text(Point::new(5., 7.), stamp));
        assert_eq!(editor.marks.len(), 1);
        let result = render(&source, &editor.marks).unwrap();
        let i = (7 * 32 + 5) * 4;
        assert_eq!(&result.pixels()[i..i + 4], &pixels[..4]);
        let i = (10 * 32 + 10) * 4;
        assert_eq!(&result.pixels()[i..i + 4], &pixels[pixels.len() - 4..]);
        editor.undo();
        assert_eq!(
            render(&source, &editor.marks).unwrap().pixels(),
            source.pixels()
        );
        editor.redo();
        assert_eq!(
            render(&source, &editor.marks).unwrap().pixels(),
            result.pixels()
        );
        let incremental = render(&render(&source, &[]).unwrap(), &editor.marks).unwrap();
        assert_eq!(incremental.pixels(), result.pixels());
    }
    #[test]
    fn crop_preserves_orientation_resolution_and_source() {
        let image = synthetic(64, 48).unwrap();
        let before = image.pixels().to_vec();
        let crop = crop(&image, r(3.2, 4.1, 9.1, 7.2), r(0., 0., 32., 24.)).unwrap();
        assert_eq!((crop.width(), crop.height()), (19, 15));
        assert_eq!(
            &crop.pixels()[..4],
            &before[(8 * 64 + 6) * 4..(8 * 64 + 6) * 4 + 4]
        );
        assert_eq!(image.pixels(), before);
    }
    #[test]
    fn png_and_dib_have_original_pixels() {
        let image = synthetic(17, 11).unwrap();
        let encoded = encode(&image).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(encoded.png))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (17, 11));
        assert_eq!(&pixels[..info.buffer_size()], image.pixels());
        assert_eq!(&encoded.dibv5[8..12], &(-11i32).to_le_bytes());
        assert_eq!(&encoded.dibv5[124..], image.bgra());
    }
    #[test]
    fn tools_visible_and_incremental_matches_full() {
        let image = synthetic(96, 80).unwrap();
        let before = image.pixels().to_vec();
        let marks: Vec<_> = [Tool::Pen, Tool::Arrow, Tool::Rectangle, Tool::Highlighter]
            .into_iter()
            .map(|tool| Mark {
                tool,
                points: vec![Point::new(12., 20.), Point::new(70., 50.)],
                color: [1., 0., 0., 1.],
                width: 3.,
                stamp: None,
            })
            .collect();
        let mut incremental = image.clone();
        for mark in &marks {
            let next = render(&incremental, std::slice::from_ref(mark)).unwrap();
            assert_ne!(incremental.pixels(), next.pixels());
            incremental = next;
        }
        assert_eq!(
            incremental.pixels(),
            render(&image, &marks).unwrap().pixels()
        );
        assert_eq!(image.pixels(), before);
    }
    #[test]
    fn invalid_buffers_rejected_and_unmarked_render_shares_pixels() {
        assert!(Image::from_rgba(20, 20, vec![0; 3]).is_err());
        assert!(validate_dimensions(usize::MAX, 4).is_err());
        let image = synthetic(10, 10).unwrap();
        assert_eq!(image.identity(), render(&image, &[]).unwrap().identity());
    }
}
