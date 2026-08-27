use std::{
    ffi::OsStr,
    fs,
    io::{Cursor, Write},
    path::Path,
    time::SystemTime,
};

use image::{
    DynamicImage, GenericImage, GrayImage, ImageFormat, ImageReader, Luma, Rgba, RgbaImage,
    imageops::{self, FilterType},
};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parser::parse;
use ratex_render::{RenderOptions, render_to_png};
use ratex_types::color::Color as RatexColor;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tracing::debug;

use crate::{
    Antialiasing, ColorScheme, Limits, PixelPadding, PixelSize, Rgb,
    config::EngineConfig,
    engine::{CacheKey, RenderFailure, RenderFailureKind},
};

const CACHE_VERSION: &[u8] = b"ratatex-ratex-v3-in-process-only";
const SUPERSAMPLE: u16 = 3;
const DENIED_COMMANDS: [&str; 30] = [
    "catcode",
    "closein",
    "closeout",
    "csname",
    "def",
    "directlua",
    "documentclass",
    "edef",
    "endcsname",
    "every",
    "gdef",
    "immediate",
    "include",
    "input",
    "let",
    "loop",
    "newcommand",
    "newread",
    "newwrite",
    "openin",
    "openout",
    "pdfobj",
    "read",
    "special",
    "usepackage",
    "write",
    "xdef",
    "shipout",
    "@@input",
    "scantokens",
];

pub(crate) struct RenderedPng {
    pub png: Vec<u8>,
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

pub(crate) fn cache_key(config: &EngineConfig, source: &str, max_columns: u16) -> CacheKey {
    let mut digest = Sha256::new();
    digest.update(CACHE_VERSION);
    digest.update(env!("CARGO_PKG_VERSION").as_bytes());
    digest.update(config.dpi.to_be_bytes());
    digest.update(config.terminal.cell.width.to_be_bytes());
    digest.update(config.terminal.cell.height.to_be_bytes());
    digest.update(max_columns.to_be_bytes());
    digest.update(config.padding.horizontal.to_be_bytes());
    digest.update(config.padding.vertical.to_be_bytes());
    digest.update(config.colors.foreground.channels());
    match config.colors.background {
        Some(background) => {
            digest.update([1]);
            digest.update(background.channels());
        }
        None => digest.update([0]),
    }
    digest.update([match config.antialiasing {
        Antialiasing::Grayscale => 0,
        Antialiasing::LcdRgb => 1,
        Antialiasing::LcdBgr => 2,
    }]);
    digest.update(source.as_bytes());
    CacheKey(digest.finalize().into())
}

pub(crate) fn validate_source(source: &str, limits: &Limits) -> Result<(), RenderFailure> {
    if source.trim().is_empty() {
        return Err(RenderFailure::new(
            RenderFailureKind::UnsafeSource,
            "formula is empty",
        ));
    }
    if source.len() > limits.max_source_bytes {
        return Err(RenderFailure::new(
            RenderFailureKind::UnsafeSource,
            format!(
                "formula is {} bytes; the configured limit is {}",
                source.len(),
                limits.max_source_bytes
            ),
        ));
    }
    if source.contains("^^")
        || source
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(RenderFailure::new(
            RenderFailureKind::UnsafeSource,
            "formula contains a denied TeX control sequence",
        ));
    }

    if let Some(command) = DENIED_COMMANDS
        .iter()
        .find(|command| contains_command(source, command))
    {
        return Err(RenderFailure::new(
            RenderFailureKind::UnsafeSource,
            format!(r"formula uses denied TeX primitive \{command}"),
        ));
    }

    let compact = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.contains(r"\begin{document}") || compact.contains(r"\end{document}") {
        return Err(RenderFailure::new(
            RenderFailureKind::UnsafeSource,
            "formula may not open or close the TeX document",
        ));
    }
    Ok(())
}

pub(crate) fn render(
    config: &EngineConfig,
    key: CacheKey,
    source: &str,
    max_columns: u16,
) -> Result<RenderedPng, RenderFailure> {
    let cache_path = config.cache_dir.join(format!("{}.png", hex::encode(key.0)));
    if let Some(rendered) = load_cached(config, &cache_path) {
        return Ok(rendered);
    }
    let raw = ratex_to_png(config, source)?;
    let rendered = postprocess(
        &raw,
        config.terminal.cell,
        max_columns,
        &config.limits,
        config.colors,
        config.antialiasing,
        config.padding,
    )?;
    if let Err(error) = store_cached(config, &cache_path, &rendered.png) {
        debug!(
            path = %cache_path.display(),
            error = %error,
            "ratatex disk cache write failed"
        );
    }
    Ok(rendered)
}

fn ratex_to_png(config: &EngineConfig, source: &str) -> Result<Vec<u8>, RenderFailure> {
    let ast = parse(source).map_err(|error| {
        RenderFailure::new(
            RenderFailureKind::Ratex,
            format!("RaTeX parser rejected the formula: {error}"),
        )
    })?;
    let layout_options = LayoutOptions::default().with_color(RatexColor::WHITE);
    let layout_box = layout(&ast, &layout_options);
    let display_list = to_display_list(&layout_box);
    let render_options = RenderOptions {
        // A 12pt TeX em at the configured DPI, rasterized at 3× resolution.
        font_size: f32::from(config.dpi) / 2.0,
        padding: 0.0,
        background_color: RatexColor::new(0.0, 0.0, 0.0, 0.0),
        font_dir: String::new(),
        device_pixel_ratio: 1.0,
    };
    render_to_png(&display_list, &render_options).map_err(|error| {
        RenderFailure::new(
            RenderFailureKind::Ratex,
            format!("RaTeX rasterizer failed: {error}"),
        )
    })
}

fn contains_command(source: &str, command: &str) -> bool {
    let needle = format!(r"\{command}");
    source.match_indices(&needle).any(|(start, _)| {
        source
            .get(start.saturating_add(needle.len())..)
            .and_then(|remaining| remaining.chars().next())
            .is_none_or(|character| !character.is_ascii_alphabetic())
    })
}

fn load_cached(config: &EngineConfig, path: &Path) -> Option<RenderedPng> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            debug!(path = %path.display(), error = %error, "ratatex cache metadata failed");
            return None;
        }
    };
    if metadata.len() == 0
        || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > config.limits.max_png_bytes
    {
        let _ = fs::remove_file(path);
        return None;
    }
    let png = match fs::read(path) {
        Ok(png) => png,
        Err(error) => {
            debug!(path = %path.display(), error = %error, "ratatex cache read failed");
            return None;
        }
    };
    match prepared_from_png(&png, config.terminal.cell, &config.limits) {
        Ok(rendered) => Some(rendered),
        Err(error) => {
            debug!(path = %path.display(), error = %error, "discarding invalid ratatex cache entry");
            let _ = fs::remove_file(path);
            None
        }
    }
}

fn store_cached(config: &EngineConfig, path: &Path, png: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(&config.cache_dir)?;
    let mut temporary = NamedTempFile::new_in(&config.cache_dir)?;
    temporary.write_all(png)?;
    temporary.flush()?;
    temporary.persist(path).map_err(|error| error.error)?;
    prune_disk_cache(&config.cache_dir, path, config.limits.max_disk_entries);
    Ok(())
}

fn prune_disk_cache(directory: &Path, keep: &Path, max_entries: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut pngs = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path == keep || path.extension() != Some(OsStr::new("png")) {
                return None;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    pngs.sort_unstable_by_key(|(modified, _)| *modified);
    let remove = pngs.len().saturating_sub(max_entries.saturating_sub(1));
    for (_, path) in pngs.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

fn postprocess(
    raw: &[u8],
    cell: PixelSize,
    max_columns: u16,
    limits: &Limits,
    colors: ColorScheme,
    antialiasing: Antialiasing,
    padding: PixelPadding,
) -> Result<RenderedPng, RenderFailure> {
    let source = decode_png(raw, limits)?;
    let (left, top, width, height) =
        alpha_bounds(&source).unwrap_or((0, 0, source.width(), source.height()));
    let alpha = GrayImage::from_fn(width, height, |x, y| {
        Luma([source.get_pixel(left + x, top + y).0[3]])
    });
    let target_width = alpha.width().div_ceil(u32::from(SUPERSAMPLE)).max(1);
    let target_height = alpha.height().div_ceil(u32::from(SUPERSAMPLE)).max(1);
    let glyph = match (antialiasing, colors.background) {
        (Antialiasing::LcdRgb, Some(background)) => lcd_image(
            &alpha,
            target_width,
            target_height,
            colors.foreground,
            background,
            false,
        ),
        (Antialiasing::LcdBgr, Some(background)) => lcd_image(
            &alpha,
            target_width,
            target_height,
            colors.foreground,
            background,
            true,
        ),
        (Antialiasing::Grayscale | Antialiasing::LcdRgb | Antialiasing::LcdBgr, _) => {
            grayscale_image(&alpha, target_width, target_height, colors.foreground)
        }
    };
    align_to_cells(
        &glyph,
        cell,
        max_columns,
        limits,
        colors.background,
        padding,
    )
}

fn alpha_bounds(source: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let mut left = source.width();
    let mut right = 0;
    let mut top = source.height();
    let mut bottom = 0;
    let mut found = false;
    for (x, y, pixel) in source.enumerate_pixels() {
        if pixel.0[3] == 0 {
            continue;
        }
        found = true;
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
    }
    found.then(|| {
        (
            left,
            top,
            right.saturating_sub(left).saturating_add(1),
            bottom.saturating_sub(top).saturating_add(1),
        )
    })
}

fn grayscale_image(alpha: &GrayImage, width: u32, height: u32, foreground: Rgb) -> RgbaImage {
    let resized = imageops::resize(alpha, width, height, FilterType::Lanczos3);
    RgbaImage::from_fn(width, height, |x, y| {
        let coverage = resized.get_pixel(x, y).0[0];
        Rgba([foreground.red, foreground.green, foreground.blue, coverage])
    })
}

fn lcd_image(
    alpha: &GrayImage,
    width: u32,
    height: u32,
    foreground: Rgb,
    background: Rgb,
    bgr: bool,
) -> RgbaImage {
    let subpixel_width = width.saturating_mul(u32::from(SUPERSAMPLE));
    let vertical = imageops::resize(alpha, subpixel_width, height, FilterType::Lanczos3);
    RgbaImage::from_fn(width, height, |x, y| {
        let base = x.saturating_mul(u32::from(SUPERSAMPLE));
        let mut coverage = [
            lcd_coverage(&vertical, base, y),
            lcd_coverage(&vertical, base.saturating_add(1), y),
            lcd_coverage(&vertical, base.saturating_add(2), y),
        ];
        if bgr {
            coverage.reverse();
        }
        let foreground = foreground.channels();
        let background = background.channels();
        let mut output = [0_u8; 4];
        for channel in 0..3 {
            output[channel] = blend(foreground[channel], background[channel], coverage[channel]);
        }
        output[3] = u8::MAX;
        Rgba(output)
    })
}

fn lcd_coverage(alpha: &GrayImage, center: u32, y: u32) -> u8 {
    const WEIGHTS: [u16; 5] = [8, 77, 86, 77, 8];
    let maximum = alpha.width().saturating_sub(1);
    let mut total = 0_u32;
    for (offset, weight) in (-2_i64..=2).zip(WEIGHTS) {
        let sample = i64::from(center)
            .saturating_add(offset)
            .clamp(0, i64::from(maximum));
        let sample = u32::try_from(sample).unwrap_or(maximum);
        total = total.saturating_add(
            u32::from(alpha.get_pixel(sample, y).0[0]).saturating_mul(u32::from(weight)),
        );
    }
    u8::try_from((total.saturating_add(128)) / 256).unwrap_or(u8::MAX)
}

fn blend(foreground: u8, background: u8, coverage: u8) -> u8 {
    let coverage = u16::from(coverage);
    let inverse = u16::from(u8::MAX).saturating_sub(coverage);
    let value = u16::from(foreground)
        .saturating_mul(coverage)
        .saturating_add(u16::from(background).saturating_mul(inverse))
        / u16::from(u8::MAX);
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn align_to_cells(
    glyph: &RgbaImage,
    cell: PixelSize,
    max_columns: u16,
    limits: &Limits,
    background: Option<Rgb>,
    padding: PixelPadding,
) -> Result<RenderedPng, RenderFailure> {
    let fill = background.map_or(Rgba([0, 0, 0, 0]), |color| {
        Rgba([color.red, color.green, color.blue, u8::MAX])
    });
    let padded_width = glyph
        .width()
        .saturating_add(u32::from(padding.horizontal).saturating_mul(2));
    let padded_height = glyph
        .height()
        .saturating_add(u32::from(padding.vertical).saturating_mul(2));
    checked_pixels(padded_width, padded_height, limits)?;
    let mut padded = RgbaImage::from_pixel(padded_width, padded_height, fill);
    padded
        .copy_from(
            glyph,
            u32::from(padding.horizontal),
            u32::from(padding.vertical),
        )
        .map_err(|error| {
            RenderFailure::new(
                RenderFailureKind::InvalidPng,
                format!("failed to pad formula PNG: {error}"),
            )
        })?;

    let max_width = u32::from(max_columns).saturating_mul(u32::from(cell.width));
    let max_height = u32::from(limits.max_rows).saturating_mul(u32::from(cell.height));
    let (fitted_width, fitted_height) =
        fit_dimensions(padded.width(), padded.height(), max_width, max_height);
    let fitted = if fitted_width == padded.width() && fitted_height == padded.height() {
        padded
    } else {
        imageops::resize(&padded, fitted_width, fitted_height, FilterType::Lanczos3)
    };

    let columns = fitted
        .width()
        .div_ceil(u32::from(cell.width))
        .clamp(1, u32::from(max_columns));
    let rows = fitted
        .height()
        .div_ceil(u32::from(cell.height))
        .clamp(1, u32::from(limits.max_rows));
    let pixel_width = columns.saturating_mul(u32::from(cell.width));
    let pixel_height = rows.saturating_mul(u32::from(cell.height));
    checked_pixels(pixel_width, pixel_height, limits)?;
    let mut aligned = RgbaImage::from_pixel(pixel_width, pixel_height, fill);
    let left = pixel_width.saturating_sub(fitted.width()) / 2;
    let top = pixel_height.saturating_sub(fitted.height()) / 2;
    aligned.copy_from(&fitted, left, top).map_err(|error| {
        RenderFailure::new(
            RenderFailureKind::InvalidPng,
            format!("failed to align formula PNG: {error}"),
        )
    })?;

    let mut png = Vec::new();
    DynamicImage::ImageRgba8(aligned)
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| {
            RenderFailure::new(
                RenderFailureKind::InvalidPng,
                format!("failed to encode formula PNG: {error}"),
            )
        })?;
    if png.len() > limits.max_png_bytes {
        return Err(RenderFailure::new(
            RenderFailureKind::InvalidPng,
            "processed formula PNG exceeded the configured byte limit",
        ));
    }
    Ok(RenderedPng {
        png,
        columns: u16::try_from(columns).unwrap_or(limits.max_columns),
        rows: u16::try_from(rows).unwrap_or(limits.max_rows),
        pixel_width,
        pixel_height,
    })
}

fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let mut width = width.max(1);
    let mut height = height.max(1);
    if width > max_width {
        height = u32::try_from(
            u64::from(height)
                .saturating_mul(u64::from(max_width))
                .div_ceil(u64::from(width)),
        )
        .unwrap_or(max_height)
        .max(1);
        width = max_width.max(1);
    }
    if height > max_height {
        width = u32::try_from(
            u64::from(width)
                .saturating_mul(u64::from(max_height))
                .div_ceil(u64::from(height)),
        )
        .unwrap_or(max_width)
        .max(1);
        height = max_height.max(1);
    }
    (width, height)
}

fn prepared_from_png(
    png: &[u8],
    cell: PixelSize,
    limits: &Limits,
) -> Result<RenderedPng, RenderFailure> {
    let image = decode_png(png, limits)?;
    if image.width() % u32::from(cell.width) != 0 || image.height() % u32::from(cell.height) != 0 {
        return Err(RenderFailure::new(
            RenderFailureKind::InvalidPng,
            "cached PNG is not aligned to terminal cells",
        ));
    }
    let columns = image.width() / u32::from(cell.width);
    let rows = image.height() / u32::from(cell.height);
    if columns == 0
        || rows == 0
        || columns > u32::from(limits.max_columns)
        || rows > u32::from(limits.max_rows)
    {
        return Err(RenderFailure::new(
            RenderFailureKind::InvalidPng,
            "cached PNG cell dimensions exceed configured bounds",
        ));
    }
    Ok(RenderedPng {
        png: png.to_vec(),
        columns: u16::try_from(columns).unwrap_or(limits.max_columns),
        rows: u16::try_from(rows).unwrap_or(limits.max_rows),
        pixel_width: image.width(),
        pixel_height: image.height(),
    })
}

fn decode_png(raw: &[u8], limits: &Limits) -> Result<RgbaImage, RenderFailure> {
    if raw.is_empty() || raw.len() > limits.max_png_bytes {
        return Err(RenderFailure::new(
            RenderFailureKind::InvalidPng,
            "PNG byte size is outside configured bounds",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(raw), ImageFormat::Png);
    let mut image_limits = image::Limits::default();
    let maximum_dimension = u32::try_from(limits.max_image_pixels)
        .unwrap_or(u32::MAX)
        .min(16_384);
    image_limits.max_image_width = Some(maximum_dimension);
    image_limits.max_image_height = Some(maximum_dimension);
    image_limits.max_alloc = Some(limits.max_image_pixels.saturating_mul(4));
    reader.limits(image_limits);
    let image = reader.decode().map_err(|error| {
        RenderFailure::new(
            RenderFailureKind::InvalidPng,
            format!("failed to decode formula PNG: {error}"),
        )
    })?;
    checked_pixels(image.width(), image.height(), limits)?;
    Ok(image.to_rgba8())
}

fn checked_pixels(width: u32, height: u32, limits: &Limits) -> Result<(), RenderFailure> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels == 0 || pixels > limits.max_image_pixels {
        return Err(RenderFailure::new(
            RenderFailureKind::InvalidPng,
            format!(
                "formula image has {pixels} pixels; the configured limit is {}",
                limits.max_image_pixels
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

    use super::{fit_dimensions, postprocess, validate_source};
    use crate::{
        Antialiasing, ColorScheme, Limits, PixelPadding, PixelSize, RenderFailureKind, Rgb,
    };

    #[test]
    fn source_validation_blocks_file_and_macro_primitives() {
        let limits = Limits::default();
        assert!(validate_source(r"\frac{a}{b}", &limits).is_ok());
        for source in [
            r"\input{/etc/passwd}",
            r"\csname input\endcsname",
            r"\write18{bad}",
            r"\end { document }",
        ] {
            let failure = validate_source(source, &limits).unwrap_err();
            assert_eq!(failure.kind(), RenderFailureKind::UnsafeSource);
        }
    }

    #[test]
    fn fitting_preserves_aspect_ratio_without_upscaling() {
        assert_eq!(fit_dimensions(100, 50, 80, 80), (80, 40));
        assert_eq!(fit_dimensions(100, 200, 80, 80), (40, 80));
        assert_eq!(fit_dimensions(20, 10, 80, 80), (20, 10));
    }

    #[test]
    fn postprocessing_aligns_transparent_grayscale_to_cells() {
        let source = RgbaImage::from_pixel(30, 18, Rgba([255, 255, 255, 255]));
        let mut raw = Vec::new();
        DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut raw), ImageFormat::Png)
            .unwrap();
        let rendered = postprocess(
            &raw,
            PixelSize::new(8, 16),
            20,
            &Limits::default(),
            ColorScheme {
                foreground: Rgb::new(240, 240, 240),
                background: None,
            },
            Antialiasing::Grayscale,
            PixelPadding::default(),
        )
        .unwrap();
        assert_eq!(rendered.pixel_width % 8, 0);
        assert_eq!(rendered.pixel_height % 16, 0);
        let image = image::load_from_memory(&rendered.png).unwrap().to_rgba8();
        assert!(image.pixels().any(|pixel| pixel.0[3] == 0));
        assert!(image.pixels().any(|pixel| pixel.0[3] > 0));
    }

    #[test]
    fn postprocessing_discards_transparent_canvas_before_reserving_rows() {
        let mut source = RgbaImage::from_pixel(90, 180, Rgba([0, 0, 0, 0]));
        for y in 72..108 {
            for x in 15..75 {
                source.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let mut raw = Vec::new();
        DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut raw), ImageFormat::Png)
            .unwrap();
        let rendered = postprocess(
            &raw,
            PixelSize::new(10, 20),
            20,
            &Limits::default(),
            ColorScheme::default(),
            Antialiasing::Grayscale,
            PixelPadding::default(),
        )
        .unwrap();

        assert_eq!(rendered.rows, 1);
        assert_eq!(rendered.pixel_height, 20);
    }

    #[test]
    fn lcd_mode_composites_an_opaque_background() {
        let source = RgbaImage::from_pixel(9, 9, Rgba([255, 255, 255, 128]));
        let mut raw = Vec::new();
        DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut raw), ImageFormat::Png)
            .unwrap();
        let rendered = postprocess(
            &raw,
            PixelSize::new(8, 16),
            20,
            &Limits::default(),
            ColorScheme {
                foreground: Rgb::new(235, 219, 178),
                background: Some(Rgb::new(29, 32, 33)),
            },
            Antialiasing::LcdRgb,
            PixelPadding::default(),
        )
        .unwrap();
        let image = image::load_from_memory(&rendered.png).unwrap().to_rgba8();
        assert!(image.pixels().all(|pixel| pixel.0[3] == u8::MAX));
    }
}
