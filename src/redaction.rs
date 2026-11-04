//! Opaque, source-independent replacement texture for privacy redaction.
//!
//! The layout resembles coarse, blurred text but contains no glyphs or source
//! image information. Generate it once on a worker, then reuse the same image
//! for preview and export. A seed belongs to the capture; it must never be
//! derived from image pixels, text, or other content being concealed.

use crate::portable_image::{Image, validate_dimensions};

pub type Seed = [u8; 32];

/// Obtain capture-specific randomness without making redaction depend on the
/// availability of system entropy. Even the fixed fallback reveals no source
/// content: all replacement pixels are generated independently of it.
pub fn fresh_seed() -> Seed {
    let mut seed = [0; 32];
    if getrandom::fill(&mut seed).is_err() {
        seed = [
            0x29, 0xd8, 0xb3, 0x71, 0x46, 0xa2, 0x0c, 0xef, 0x95, 0x63, 0x7a, 0x18, 0xc4, 0x5e,
            0xf0, 0x82, 0x6b, 0x13, 0xad, 0x97, 0x30, 0xe6, 0x59, 0x42, 0xbe, 0x74, 0x08, 0xcd,
            0x51, 0x9f, 0xe3, 0x26,
        ];
    }
    seed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: [u8; 3],
}

impl Palette {
    pub const DARK: Self = Self {
        background: [29, 27, 34],
    };
    pub const LIGHT: Self = Self {
        background: [246, 245, 243],
    };
}

/// Estimate a quiet background from caller-supplied SAFE, premultiplied RGBA
/// samples. The caller must collect them outside the entire exported crop,
/// never from masked content. This function does not inspect an image.
///
/// A dominant neutral luminance band rejects isolated text and colored icons.
/// Transparent or strongly colored samples cannot select the background.
pub fn palette_from_samples(samples: &[[u8; 4]]) -> Palette {
    #[derive(Clone, Copy, Default)]
    struct Bucket {
        count: u128,
        rgb: [u128; 3],
    }

    let mut buckets = [Bucket::default(); 32];
    for &[r, g, b, a] in samples {
        let low = r.min(g).min(b);
        let high = r.max(g).max(b);
        if a < 240 || high - low > 48 {
            continue;
        }
        let bucket = &mut buckets[luminance([r, g, b]) as usize / 8];
        bucket.count += 1;
        for (sum, channel) in bucket.rgb.iter_mut().zip([r, g, b]) {
            *sum += channel as u128;
        }
    }

    // Include adjacent bands so a background near a quantization boundary is
    // not split into competing groups. Ties have a fixed, deterministic order.
    let mut selected = 0;
    let mut selected_count = 0;
    for center in 0..buckets.len() {
        let count = buckets[center.saturating_sub(1)..=(center + 1).min(31)]
            .iter()
            .map(|bucket| bucket.count)
            .sum();
        if count > selected_count {
            selected = center;
            selected_count = count;
        }
    }
    if selected_count == 0 {
        return Palette::DARK;
    }

    let mut rgb = [0_u128; 3];
    for bucket in &buckets[selected.saturating_sub(1)..=(selected + 1).min(31)] {
        for (sum, channel) in rgb.iter_mut().zip(bucket.rgb) {
            *sum += channel;
        }
    }
    let average = rgb.map(|sum| ((sum + selected_count / 2) / selected_count) as u8);
    let gray = luminance(average) as u16;
    Palette {
        // Retain a little surface hue without turning the texture into color
        // noise when a low-saturation control appears in the safe samples.
        background: average.map(|channel| ((channel as u16 * 3 + gray + 2) / 4) as u8),
    }
}

/// Generate a replacement image without reading any source image or pixels.
///
/// Every pixel is opaque, so RGBA and premultiplied RGBA are identical here.
/// Layout is anchored at (0, 0): identical seed, cell size, and palette give
/// identical pixels at shared coordinates even if the canvas dimensions grow.
/// `cell` is the coarse mosaic block size in pixels and must be nonzero.
pub fn generate(
    width: usize,
    height: usize,
    cell: usize,
    seed: Seed,
    palette: Palette,
) -> Result<Image, String> {
    validate_dimensions(width, height)?;
    if cell == 0 {
        return Err("Redaction cell size must be at least one pixel".into());
    }

    let mut rgba = Vec::new();
    rgba.try_reserve_exact(width * height * 4)
        .map_err(|_| "Could not allocate redaction image")?;
    let mut background_row = vec![0; width * 4];
    for pixel in background_row.as_chunks_mut::<4>().0 {
        *pixel = [
            palette.background[0],
            palette.background[1],
            palette.background[2],
            255,
        ];
    }
    let mut row = background_row.clone();
    let field = Field::new(seed);
    let columns = width.div_ceil(cell);
    for grid_y in 0..height.div_ceil(cell) {
        row.copy_from_slice(&background_row);
        draw_row(&mut row, columns, cell, grid_y, &field, palette);
        let repeated_rows = cell.min(height - grid_y * cell);
        for _ in 0..repeated_rows {
            rgba.extend_from_slice(&row);
        }
    }
    Image::from_premultiplied_rgba(width, height, rgba)
}

fn luminance([r, g, b]: [u8; 3]) -> u8 {
    ((r as u16 * 54 + g as u16 * 183 + b as u16 * 19 + 128) / 256) as u8
}

// This small deterministic field is for visual layout, not cryptography. Source
// independence, opacity, and the compositing mask provide the privacy property.
struct Field([u64; 4]);

impl Field {
    fn new(seed: Seed) -> Self {
        let mut keys = [0; 4];
        for (i, bytes) in seed.as_chunks::<8>().0.iter().enumerate() {
            keys[i] =
                mix(u64::from_le_bytes(*bytes) ^ (i as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15));
        }
        Self(keys)
    }

    fn band(&self, index: usize) -> u64 {
        mix(self.0[0]
            ^ mix(index as u64 ^ self.0[1])
            ^ self.0[2].rotate_left(23)
            ^ self.0[3].rotate_left(41))
    }
}

fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

struct Layout(u64);

impl Layout {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        mix(self.0)
    }
}

fn draw_row(
    row: &mut [u8],
    columns: usize,
    cell: usize,
    grid_y: usize,
    field: &Field,
    palette: Palette,
) {
    const BAND_ROWS: usize = 8;
    let band = grid_y / BAND_ROWS;
    let local_y = grid_y % BAND_ROWS;
    let band_key = field.band(band);
    if band > 0 && band_key.is_multiple_of(13) {
        return; // Occasional paragraph space, unrelated to the concealed text.
    }
    let top = if band == 0 {
        0
    } else {
        1 + (band_key % 3) as usize
    };
    if local_y + 1 < top || local_y > top + 3 {
        return;
    }

    let mut layout = Layout(band_key);
    // The first word can extend beyond the left edge, avoiding a visible frame
    // around a redaction and giving even very small images some texture.
    let mut x = -((layout.next() % 4) as isize);
    let dark = luminance(palette.background) < 128;
    while x < columns as isize {
        let length = 2 + (layout.next() % 5 + layout.next() % 7) as usize;
        let word_key = layout.next();
        let gap = if word_key.is_multiple_of(17) {
            4 + (layout.next() % 5) as usize
        } else {
            1 + (layout.next() % 3) as usize
        };
        let word_top = top + usize::from(word_key.is_multiple_of(5));
        let word_height = 3 - (word_top - top);
        let vertical = if local_y + 1 == word_top || local_y == word_top + word_height {
            13
        } else if (word_top..word_top + word_height).contains(&local_y) {
            if local_y == top + 1 { 100 } else { 72 }
        } else {
            0
        };
        if vertical > 0 {
            let contrast = if dark {
                52 + (word_key % 28) as i16
            } else {
                70 + (word_key % 40) as i16
            };
            let blue = word_key.is_multiple_of(29);
            let ink = std::array::from_fn::<_, 3, _>(|channel| {
                let tint = if blue { [-12, 0, 27][channel] } else { 0 };
                (palette.background[channel] as i16
                    + if dark { contrast } else { -contrast }
                    + tint)
                    .clamp(0, 255) as u8
            });
            let start = x.max(0) as usize;
            let end = (x + length as isize).max(0).min(columns as isize) as usize;
            for column in start..end {
                let word_x = (column as isize - x) as usize;
                let column_key = mix(word_key ^ (word_x as u64).wrapping_mul(0xd6e8feb86659fd93));
                let cell_key = mix(column_key ^ (local_y as u64).wrapping_mul(0xa0761d6478bd642f));
                let density = 40 + column_key % 61;
                let variation = 65 + cell_key % 36;
                let edge = if word_x == 0 || word_x + 1 == length {
                    75
                } else {
                    100
                };
                let coverage = (vertical * density * variation * edge / 1_000_000) as u16;
                let color = std::array::from_fn::<_, 3, _>(|channel| {
                    ((palette.background[channel] as u16 * (100 - coverage)
                        + ink[channel] as u16 * coverage
                        + 50)
                        / 100) as u8
                });
                let pixel_start = column * cell;
                let pixel_end = pixel_start.saturating_add(cell).min(row.len() / 4);
                for pixel in row[pixel_start * 4..pixel_end * 4].as_chunks_mut::<4>().0 {
                    *pixel = [color[0], color[1], color[2], 255];
                }
            }
        }
        x += (length + gap) as isize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_texture_is_repeatable_and_opaque() {
        let seed = [0x73; 32];
        let first = generate(317, 193, 7, seed, Palette::DARK).unwrap();
        let second = generate(317, 193, 7, seed, Palette::DARK).unwrap();
        assert_eq!(first.pixels(), second.pixels());
        for (rgba, bgra) in first
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(first.bgra().as_chunks::<4>().0)
        {
            assert_eq!(rgba[3], 255);
            assert_eq!(*bgra, [rgba[2], rgba[1], rgba[0], 255]);
        }
    }

    #[test]
    fn independent_seeds_change_the_texture() {
        let first = generate(317, 193, 7, [0; 32], Palette::DARK).unwrap();
        let second = generate(317, 193, 7, [1; 32], Palette::DARK).unwrap();
        assert_ne!(first.pixels(), second.pixels());

        let mut last_byte = [0; 32];
        last_byte[31] = 1;
        let third = generate(317, 193, 7, last_byte, Palette::DARK).unwrap();
        assert_ne!(first.pixels(), third.pixels());
    }

    #[test]
    fn coordinates_are_stable_when_canvas_dimensions_change() {
        let small = generate(121, 73, 6, [91; 32], Palette::LIGHT).unwrap();
        let large = generate(197, 119, 6, [91; 32], Palette::LIGHT).unwrap();
        for y in 0..small.height() {
            assert_eq!(
                &small.pixels()[y * small.width() * 4..(y + 1) * small.width() * 4],
                &large.pixels()[y * large.width() * 4..(y * large.width() + small.width()) * 4]
            );
        }
    }

    #[test]
    fn tiny_and_partial_cells_have_exact_dimensions() {
        for (width, height, cell) in [(1, 1, 1), (1, 3, 8), (13, 7, 3), (7, 13, usize::MAX)] {
            let image = generate(width, height, cell, [7; 32], Palette::DARK).unwrap();
            assert_eq!((image.width(), image.height()), (width, height));
            assert_eq!(image.pixels().len(), width * height * 4);
            assert!(
                image
                    .pixels()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p[3] == 255)
            );
        }
    }

    #[test]
    fn invalid_dimensions_and_empty_cells_are_rejected() {
        for (width, height, cell) in [
            (0, 1, 8),
            (1, 0, 8),
            (32769, 1, 8),
            (32768, 4097, 8),
            (1, 1, 0),
        ] {
            assert!(generate(width, height, cell, [0; 32], Palette::DARK).is_err());
        }
    }

    #[test]
    fn palette_resists_text_icons_and_transparent_samples() {
        let mut samples = vec![[30, 27, 35, 255]; 128];
        samples.extend([[230, 228, 230, 255]; 16]);
        samples.extend([[20, 100, 230, 255]; 32]);
        samples.extend([[255, 255, 255, 0]; 128]);
        let palette = palette_from_samples(&samples);
        for (actual, expected) in palette.background.into_iter().zip([30, 27, 35]) {
            assert!(actual.abs_diff(expected) <= 3);
        }
    }

    #[test]
    fn palette_handles_light_surfaces_and_luminance_boundaries() {
        let mut samples = vec![[246, 245, 243, 255]; 32];
        samples.extend([[24, 24, 24, 255]; 8]);
        let light = palette_from_samples(&samples);
        assert!(light.background.into_iter().all(|channel| channel > 240));

        let mut samples = vec![[31, 31, 31, 255]; 16];
        samples.extend([[32, 32, 32, 255]; 16]);
        samples.extend([[210, 210, 210, 255]; 20]);
        let dark = palette_from_samples(&samples);
        assert!(
            dark.background
                .into_iter()
                .all(|channel| (31..=32).contains(&channel))
        );
    }

    #[test]
    fn unusable_palette_samples_have_a_safe_default() {
        assert_eq!(palette_from_samples(&[]), Palette::DARK);
        assert_eq!(
            palette_from_samples(&[[255, 0, 255, 255], [255, 255, 255, 0], [0, 0, 0, 127]]),
            Palette::DARK
        );
    }
}
