//! Opt-in, deterministic image-pipeline benchmark. It never captures the screen
//! or touches a pasteboard. Run `cargo run --release --bin still-bench`.
use std::hint::black_box;
use std::time::Instant;

use objc2_core_foundation::{CFDictionary, CFMutableData, CFNumber, CFRetained, CFString};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow,
    CGBitmapContextGetData, CGColorSpace, CGContext, CGImageAlphaInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_image_io::{
    CGImageDestination, kCGImagePropertyPNGCompressionFilter, kCGImagePropertyPNGDictionary,
};
use still::imaging::{self, Image, Mark, Tool};

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

struct Measurement {
    median: f64,
    p95: f64,
}

fn measure(mut operation: impl FnMut(), iterations: usize, batch: usize) -> Measurement {
    operation();
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        for _ in 0..batch {
            operation();
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0 / batch as f64);
    }
    samples.sort_by(f64::total_cmp);
    Measurement {
        median: samples[samples.len() / 2],
        p95: samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)],
    }
}

fn marks(width: usize, height: usize) -> Vec<Mark> {
    (0..48)
        .map(|index| {
            let x = width as f64 * (0.08 + (index % 8) as f64 * 0.105);
            let y = height as f64 * (0.10 + (index / 8) as f64 * 0.14);
            Mark {
                tool: [Tool::Pen, Tool::Arrow, Tool::Rectangle, Tool::Highlighter][index % 4],
                points: (0..80)
                    .map(|point| {
                        NSPoint::new(
                            x + point as f64 * width as f64 / 2500.0,
                            y + (point as f64 * 0.1).sin() * height as f64 / 100.0,
                        )
                    })
                    .collect(),
                color: [1.0, 0.39, 0.34, 1.0],
                width: width as f64 / 640.0,
                stamp: None,
            }
        })
        .collect()
}

fn system_png(image: &Image, filter: Option<i64>) -> Vec<u8> {
    let data = CFMutableData::new(None, 0).expect("encode buffer");
    let identifier = CFString::from_str("public.png");
    let value = CFNumber::new_i64(filter.unwrap_or(0));
    // SAFETY: The framework constant has process lifetime. Dictionary keys are
    // CFStrings and the PNG filter value is the documented long CFNumber type.
    let properties = CFDictionary::from_slices(
        &[unsafe { kCGImagePropertyPNGCompressionFilter }],
        &[&*value],
    );
    let properties =
        CFDictionary::from_slices(&[unsafe { kCGImagePropertyPNGDictionary }], &[&*properties]);
    let destination = unsafe { CGImageDestination::with_data(&data, &identifier, 1, None) }
        .expect("PNG destination");
    unsafe {
        destination.add_image(image.cg_image(), filter.map(|_| properties.as_opaque()));
        assert!(destination.finalize());
    }
    data.to_vec()
}

fn bitmap(width: usize, height: usize) -> CFRetained<CGContext> {
    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
    // SAFETY: Null data requests owned allocation; dimensions are fixed small
    // benchmark sizes. The resulting RGBA buffer lives with the context.
    unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            0,
            Some(&color_space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .unwrap()
}

/// Deterministic high-entropy scene: smoothly varying colored regions with
/// per-pixel texture. This is a compression stress fixture, not a photograph.
fn texture(width: usize, height: usize) -> Image {
    let context = bitmap(width, height);
    let stride = CGBitmapContextGetBytesPerRow(Some(&context));
    // SAFETY: The context owns stride×height writable bytes and no image yet
    // aliases this storage. Its lifetime covers this temporary mutable slice.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            CGBitmapContextGetData(Some(&context)).cast::<u8>(),
            stride * height,
        )
    };
    let mut random = 0x5eeda11ce05eed_u64;
    for y in 0..height {
        for x in 0..width {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let noise = (random & 63) as u8;
            let pixel = &mut bytes[y * stride + x * 4..][..4];
            pixel[0] = ((x * 160 / width) as u8).saturating_add(noise);
            pixel[1] = ((y * 160 / height) as u8).saturating_add((random >> 8 & 63) as u8);
            pixel[2] =
                (((x / 40 + y / 40) % 4 * 42) as u8).saturating_add((random >> 16 & 63) as u8);
            pixel[3] = 255;
        }
    }
    Image::from_cg(CGBitmapContextCreateImage(Some(&context)).unwrap()).unwrap()
}

fn stroke_study() {
    let context = bitmap(3456, 2234);
    for count in [32, 128, 512] {
        let strokes: Vec<_> = (0..count)
            .map(|stroke| Mark {
                tool: Tool::Pen,
                points: (0..256)
                    .map(|point| {
                        NSPoint::new(
                            100.0 + (stroke % 16) as f64 * 180.0 + point as f64 * 0.5,
                            80.0 + (stroke / 16) as f64 * 55.0 + (point as f64 * 0.04).sin() * 20.0,
                        )
                    })
                    .collect(),
                color: [1.0, 0.4, 0.3, 1.0],
                width: 6.0,
                stamp: None,
            })
            .collect();
        let measurement = measure(
            || {
                for mark in &strokes {
                    mark.draw(&context);
                }
            },
            21,
            1,
        );
        println!(
            "Committed-stroke replay ({count}×256 points, no UI/background): median {:.2} ms, p95 {:.2} ms",
            measurement.median, measurement.p95
        );
        let base = imaging::synthetic(3456, 2234).unwrap();
        let cached = imaging::render(&base, &strokes).unwrap();
        let active = strokes[0].clone();
        let cached_timing = measure(
            || {
                CGContext::draw_image(
                    Some(&context),
                    rect(0.0, 0.0, 3456.0, 2234.0),
                    Some(cached.cg_image()),
                );
                active.draw(&context);
            },
            21,
            1,
        );
        println!(
            "Cached raster + one active stroke ({count} committed): median {:.2} ms, p95 {:.2} ms",
            cached_timing.median, cached_timing.p95
        );
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut output = None;
    let mut iterations = 21;
    let mut filter_study = false;
    let mut include_texture = false;
    let mut include_strokes = false;
    let mut compare_png = false;
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--json" => output = Some(args.next().ok_or("--json needs a file path")?.clone()),
            "--iterations" => {
                iterations = args
                    .next()
                    .ok_or("--iterations needs a count")?
                    .parse::<usize>()
                    .map_err(|_| "Invalid iteration count")?;
                if !(5..=100).contains(&iterations) {
                    return Err("Use 5–100 iterations.".into());
                }
            }
            "--filter-study" => filter_study = true,
            "--texture" => include_texture = true,
            "--stroke-study" => include_strokes = true,
            "--compare-png" => compare_png = true,
            "--help" => {
                println!(
                    "still-bench [--iterations 5..100] [--json file] [--filter-study] [--texture] [--stroke-study] [--compare-png]\nSynthetic images only; no screen capture or clipboard access."
                );
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {argument}")),
        }
    }
    let mut entries = Vec::new();
    println!(
        "Still image pipeline · release={} · {iterations} samples · synthetic desktop, 48 marks",
        !cfg!(debug_assertions)
    );
    let mut cases = vec![
        ("crop_1k", 1000, 700),
        ("retina", 3456, 2234),
        ("4k", 3840, 2160),
    ];
    if include_texture {
        cases.push(("texture_4k", 3840, 2160));
    }
    for (label, width, height) in cases {
        let textured = label == "texture_4k";
        let iterations = if textured {
            iterations.min(7)
        } else {
            iterations
        };
        let image = if textured {
            texture(width, height)
        } else {
            imaging::synthetic(width, height)?
        };
        let annotations = marks(width, height);
        let marked = imaging::render(&image, &annotations)?;
        let bounds = rect(0.0, 0.0, width as f64 / 2.0, height as f64 / 2.0);
        let selection = rect(20.25, 15.5, width as f64 / 4.0, height as f64 / 4.0);
        let crop = measure(
            || {
                black_box(imaging::crop(&image, selection, bounds).unwrap());
            },
            iterations,
            100,
        );
        let render = measure(
            || {
                black_box(imaging::render(&image, &annotations).unwrap());
            },
            iterations,
            1,
        );
        let png = measure(
            || {
                black_box(imaging::encode_png(&marked).unwrap());
            },
            iterations,
            1,
        );
        let tiff = measure(
            || {
                black_box(imaging::encode_tiff(&marked).unwrap());
            },
            iterations,
            1,
        );
        let system = compare_png.then(|| {
            measure(
                || {
                    black_box(system_png(&marked, None));
                },
                iterations,
                1,
            )
        });
        let encoded = imaging::encode(&marked)?;
        println!(
            "{label} {width}×{height} ({iterations} samples): median / p95 ms — crop {:.3}/{:.3}, render {:.2}/{:.2}, PNG {:.2}/{:.2}, TIFF {:.2}/{:.2}",
            crop.median,
            crop.p95,
            render.median,
            render.p95,
            png.median,
            png.p95,
            tiff.median,
            tiff.p95
        );
        let baseline_json = if let Some(system) = system {
            let baseline_bytes = system_png(&marked, None).len();
            println!(
                "  ImageIO PNG baseline: median {:.2} ms, p95 {:.2} ms; Rust PNG {:.2}× faster; bytes {} vs {}",
                system.median,
                system.p95,
                system.median / png.median,
                encoded.png.len(),
                baseline_bytes
            );
            format!(
                ",\"png_imageio_ms\":{{\"median\":{:.6},\"p95\":{:.6}}},\"png_imageio_bytes\":{}",
                system.median, system.p95, baseline_bytes
            )
        } else {
            String::new()
        };
        entries.push(format!("{{\"case\":\"{label}\",\"samples\":{iterations},\"width\":{width},\"height\":{height},\"crop_ms\":{{\"median\":{:.6},\"p95\":{:.6}}},\"render_ms\":{{\"median\":{:.6},\"p95\":{:.6}}},\"png_ms\":{{\"median\":{:.6},\"p95\":{:.6}}},\"tiff_ms\":{{\"median\":{:.6},\"p95\":{:.6}}},\"png_bytes\":{},\"tiff_bytes\":{}{baseline_json}}}", crop.median,crop.p95,render.median,render.p95,png.median,png.p95,tiff.median,tiff.p95,encoded.png.len(),encoded.tiff.len()));
        if filter_study && matches!(label, "4k" | "texture_4k") {
            for (name, filter) in [("none", 0x08), ("sub", 0x10), ("up", 0x20), ("all", 0xf8)] {
                let timing = measure(
                    || {
                        black_box(system_png(&marked, Some(filter)));
                    },
                    7,
                    1,
                );
                println!(
                    "PNG filter {name}: median {:.2} ms, p95 {:.2} ms, {} bytes",
                    timing.median,
                    timing.p95,
                    system_png(&marked, Some(filter)).len()
                );
            }
        }
    }
    if include_strokes {
        stroke_study();
    }
    if let Some(path) = output {
        let json = format!(
            "{{\"benchmark\":\"still-image-pipeline\",\"release\":{},\"samples\":{iterations},\"marks\":48,\"png_backend\":\"rust-png-fast\",\"content\":\"deterministic synthetic desktop; optional texture is a high-entropy stress fixture\",\"results\":[{}]}}\n",
            !cfg!(debug_assertions),
            entries.join(",")
        );
        std::fs::write(&path, json).map_err(|error| format!("Couldn't write {path}: {error}"))?;
        println!("Saved {path}");
    }
    Ok(())
}

pub fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
