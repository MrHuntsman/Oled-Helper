//! text-raster — pure-Rust glyph rasterization core shared by the hook DLL
//! and the in-app preview.
//!
//! This is the replacement for GDI's ClearType rasterizer in the targeted
//! processes: text is shaped and rasterized with `swash` (no C toolchain),
//! producing 8-bit grayscale coverage (`Format::Alpha`) or monochrome coverage
//! for crisp mode. Subpixel (RGB/BGR) coverage reuses the same pipeline later
//! via `Format::Subpixel`.
//!
//! The module is deliberately free of Win32 code so it can be unit-tested in
//! isolation.

use swash::proxy::MetricsProxy;
pub use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::{Direction, ShapeContext};
use swash::text::Script;
use swash::zeno::{Format, Vector};
use swash::FontRef;

/// Per-draw tuning knobs shared by the preview and the Explorer hook. All
/// values are 0..=100; defaults match the historical behavior (full LCD
/// filter, no crosstalk compensation, no faux bold).
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    /// LCD filter strength: interpolates the FreeType 5-tap FIR between the
    /// identity filter (0, raw box-filter coverage) and the full lcddefault
    /// filter (100). Only affects subpixel modes.
    pub lcd_strength: u8,
    /// WOLED white-subpixel crosstalk compensation: boosts the chroma of
    /// fringe pixels so the panel's RGB→RGBW conversion does not wash the
    /// color out. Luminance-preserving; neutral glyph-body pixels are
    /// untouched (unlike a blanket min() subtraction). Only affects subpixel
    /// modes.
    pub crosstalk: u8,
    /// Faux bold: swash's outline embolden, 0..=100 mapped to 0..1px of
    /// stroke thickening. Works on all modes.
    pub boldness: u8,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            lcd_strength: 100,
            crosstalk: 0,
            boldness: 0,
        }
    }
}

/// How the current mode maps to rasterization. Mode 0 (passthrough) never
/// reaches this code — the caller forwards to GDI instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderMode {
    /// Force grayscale antialiasing (Format::Alpha, 8-bit coverage).
    Grayscale,
    /// Force no antialiasing: threshold the coverage to monochrome.
    Crisp,
    /// Subpixel antialiasing for an RGB panel (per-channel coverage).
    SubpixelRgb,
    /// Subpixel antialiasing for a BGR panel (per-channel coverage).
    SubpixelBgr,
    /// WOLED RGBW stripe (R, G, W, B): R/G/B placed across a 4-subpixel pixel,
    /// ignoring the white subpixel.
    SubpixelRgwb,
    /// WOLED RGBW stripe (R, W, B, G): R/G/B placed across a 4-subpixel pixel,
    /// ignoring the white subpixel.
    SubpixelRwbg,
}

/// A single rasterized glyph, positioned relative to the run's baseline origin.
#[derive(Clone, Debug)]
pub struct PlacedGlyph {
    /// Pen position (pixels) of the glyph within the run.
    pub x: f32,
    pub y: f32,
    /// Offset of the coverage bitmap relative to the glyph origin.
    pub bitmap_left: i32,
    pub bitmap_top: i32,
    pub width: u32,
    pub height: u32,
    /// 8-bit coverage, `width * height` bytes, row-major.
    pub coverage: Vec<u8>,
}

/// The result of rasterizing a single run of text.
#[derive(Clone, Debug)]
pub struct RasterizedText {
    pub glyphs: Vec<PlacedGlyph>,
    /// Total advance width of the run in pixels.
    pub advance: f32,
    /// Font ascent (baseline to top of the alignment box) in pixels.
    pub ascent: f32,
    /// Font descent (baseline to bottom of the alignment box) in pixels.
    pub descent: f32,
    /// Pixel format of every glyph's `coverage` buffer (Mask = 1 byte/pixel
    /// alpha; SubpixelMask = 4 bytes/pixel RGBA).
    pub content: Content,
}

/// Map a render mode to its swash output format and resulting content type.
fn mode_format(mode: RenderMode) -> (Format, Content) {
    match mode {
        RenderMode::Grayscale | RenderMode::Crisp => (Format::Alpha, Content::Mask),
        RenderMode::SubpixelRgb => (Format::Subpixel, Content::SubpixelMask),
        RenderMode::SubpixelBgr => (Format::subpixel_bgra(), Content::SubpixelMask),
        // WOLED RGBW: 4 subpixels per pixel, so the R/G/B subpixel centers sit
        // at ±0.375/±0.125 instead of the RGB stripe's ±1/3. The white
        // subpixel is not sampled (the panel's scaler derives it from RGB).
        RenderMode::SubpixelRgwb => (
            Format::CustomSubpixel([-0.375, -0.125, 0.375]),
            Content::SubpixelMask,
        ),
        RenderMode::SubpixelRwbg => (
            Format::CustomSubpixel([-0.375, 0.375, 0.125]),
            Content::SubpixelMask,
        ),
    }
}

/// FreeType's `lcddefault` filter weights (sum 272, slightly over-unity to
/// compensate the low-pass energy loss, matching FreeType).
const LCD_FILTER_FULL: [f32; 5] = [16.0, 64.0, 112.0, 64.0, 16.0];
/// The identity filter (strength 0): passes coverage through unchanged.
const LCD_FILTER_IDENTITY: [f32; 5] = [0.0, 0.0, 256.0, 0.0, 0.0];

/// FreeType's `lcddefault` filter: a 5-tap horizontal FIR applied to the
/// subpixel coverage stream (R, G, B interleaved), dividing by 256. The
/// weights sum to 272, so the slight over-unity gain compensates the low-pass
/// energy loss (matching FreeType). Because each tap advances one SUBPIXEL
/// rather than one pixel, the filter spans roughly 1.7 pixels and only softens
/// the color fringing at glyph edges instead of blurring the glyph shape. Edges
/// are zero-padded.
///
/// `strength` (0..=100) interpolates between the identity filter (raw
/// coverage, harsher fringing) and the full lcddefault filter, so it doubles
/// as the filter's on/off.
fn apply_lcd_filter(coverage: &mut [u8], width: u32, height: u32, strength: u8) {
    if strength == 0 {
        return;
    }
    let t = strength as f32 / 100.0;
    let weights: [u32; 5] = std::array::from_fn(|i| {
        (LCD_FILTER_FULL[i] * t + LCD_FILTER_IDENTITY[i] * (1.0 - t)).round() as u32
    });
    let w = width as usize;
    for y in 0..height as usize {
        let row = y * w * 4;
        // Flatten this row's three color channels into a subpixel stream so the
        // FIR operates across R/G/B boundaries (that is what reduces fringing).
        let mut flat = Vec::with_capacity(w * 3);
        for x in 0..w {
            flat.push(coverage[row + x * 4]);
            flat.push(coverage[row + x * 4 + 1]);
            flat.push(coverage[row + x * 4 + 2]);
        }
        // Read taps from an unmodified snapshot so earlier writes do not feed
        // back into later taps (the filter must be a true FIR).
        let original = flat.clone();
        for i in 0..flat.len() {
            let mut acc: u32 = 0;
            for (tap, &wt) in weights.iter().enumerate() {
                let idx = i as isize + tap as isize - 2;
                if idx >= 0 && (idx as usize) < original.len() {
                    acc += wt * original[idx as usize] as u32;
                }
            }
            flat[i] = (acc / 256).min(255) as u8;
        }
        for x in 0..w {
            coverage[row + x * 4] = flat[x * 3];
            coverage[row + x * 4 + 1] = flat[x * 3 + 1];
            coverage[row + x * 4 + 2] = flat[x * 3 + 2];
        }
    }
}

/// WOLED white-subpixel crosstalk compensation. The panel derives the W
/// subpixel from the neutral component of each pixel, which washes colored
/// fringes toward white. This pushes each channel away from the pixel's mean
/// (a saturation boost) so the fringes stay colored after the RGB→RGBW
/// conversion. Because it spreads around the mean, total luminance is
/// preserved and pixels where all channels are equal (the glyph body) are
/// untouched — the failure mode of the earlier blanket min() subtraction.
fn apply_crosstalk(coverage: &mut [u8], width: u32, height: u32, strength: u8) {
    if strength == 0 {
        return;
    }
    // Max chroma amplification factor at full strength.
    let k = strength as f32 / 100.0 * 1.2;
    let n = width as usize * height as usize;
    for i in 0..n {
        let p = i * 4;
        let r = coverage[p] as f32;
        let g = coverage[p + 1] as f32;
        let b = coverage[p + 2] as f32;
        let mean = (r + g + b) / 3.0;
        let spread = (r - mean).abs().max((g - mean).abs()).max((b - mean).abs());
        if spread < 0.5 {
            continue; // neutral pixel (glyph body), nothing to compensate
        }
        coverage[p] = (mean + (r - mean) * (1.0 + k)).clamp(0.0, 255.0) as u8;
        coverage[p + 1] = (mean + (g - mean) * (1.0 + k)).clamp(0.0, 255.0) as u8;
        coverage[p + 2] = (mean + (b - mean) * (1.0 + k)).clamp(0.0, 255.0) as u8;
    }
}

/// Shape and rasterize `text` with a single font at `pixel_size`.
///
/// `font_data` is the raw bytes of the font file and `font_index` selects the
/// face within a TTC. `mode` controls grayscale vs crisp output and `opts`
/// carries the per-draw knobs (LCD filter strength, crosstalk, faux bold).
/// Returns `None` if the font cannot be parsed.
pub fn rasterize(
    font_data: &[u8],
    font_index: u32,
    pixel_size: f32,
    text: &str,
    mode: RenderMode,
    opts: RenderOptions,
) -> Option<RasterizedText> {
    if text.is_empty() {
        return Some(RasterizedText {
            glyphs: Vec::new(),
            advance: 0.0,
            ascent: 0.0,
            descent: 0.0,
            content: mode_format(mode).1,
        });
    }

    let font = FontRef::from_index(font_data, font_index as usize)?;
    let metrics = MetricsProxy::from_font(&font)
        .materialize_metrics(&font, &[])
        .scale(pixel_size);

    // Shape the run. Milestone 1 assumes a single left-to-right Latin run;
    // script/direction itemization is added later.
    let mut shape_context = ShapeContext::new();
    let mut shaper = shape_context
        .builder(font)
        .script(Script::Latin)
        .direction(Direction::LeftToRight)
        .size(pixel_size)
        .build();
    shaper.add_str(text);

    let (format, content) = mode_format(mode);

    // One scale context/scaler for the whole run (same size + hinting).
    let mut scale_context = ScaleContext::new();
    let mut scaler = scale_context
        .builder(font)
        .size(pixel_size)
        .hint(true)
        .build();

    // Faux bold: 0..=100 maps to 0..1px of outline stroke thickening.
    let embolden = opts.boldness as f32 / 100.0;

    let mut glyphs: Vec<PlacedGlyph> = Vec::new();
    let mut pen = 0.0f32;
    let mut failed = false;

    shaper.shape_with(|cluster| {
        if failed {
            return;
        }
        for glyph in cluster.glyphs {
            // swash reports x/y relative to the run origin (x is 0 for base
            // glyphs), so accumulate the advance into a pen to position each
            // glyph; marks already carry their base-relative x.
            let gx = pen + glyph.x;
            // Fractional subpixel phase for this glyph. Grayscale still applies
            // it so coverage lands smoothly; crisp is thresholded afterward.
            let offset = Vector::new(gx.fract(), glyph.y.fract());
            let sources = [
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ];
            let mut render = Render::new(&sources);
            render.format(format);
            render.offset(offset);
            if embolden > 0.0 {
                render.embolden(embolden);
            }
            let image = render.render(&mut scaler, glyph.id);
            let Some(image) = image else {
                continue;
            };
            if image.placement.width == 0 || image.placement.height == 0 {
                continue;
            }
            // Only accept the coverage formats we know how to composite. Color
            // (BGRA) or bitmap strikes use a different byte layout; bail out so
            // the caller falls back to GDI rather than misread the buffer.
            if !matches!(image.content, Content::Mask | Content::SubpixelMask) {
                failed = true;
                return;
            }
            let bpp = match image.content {
                Content::Mask => 1,
                _ => 4,
            };
            if image.data.len()
                != (image.placement.width * image.placement.height * bpp) as usize
            {
                failed = true;
                return;
            }
            let mut data = image.data;
            if matches!(image.content, Content::SubpixelMask) {
                apply_lcd_filter(&mut data, image.placement.width, image.placement.height, opts.lcd_strength);
                apply_crosstalk(&mut data, image.placement.width, image.placement.height, opts.crosstalk);
            }
            let coverage = match mode {
                RenderMode::Crisp => data
                    .into_iter()
                    .map(|a| if a >= 128 { 255 } else { 0 })
                    .collect(),
                _ => data,
            };
            glyphs.push(PlacedGlyph {
                x: gx,
                y: glyph.y,
                bitmap_left: image.placement.left,
                bitmap_top: image.placement.top,
                width: image.placement.width,
                height: image.placement.height,
                coverage,
            });
            pen += glyph.advance;
        }
    });

    if failed {
        return None;
    }
    Some(RasterizedText {
        glyphs,
        advance: pen,
        ascent: metrics.ascent,
        descent: metrics.descent,
        content,
    })
}

/// Rasterize a run of glyph indices (GDI's `ETO_GLYPH_INDEX` path: the string
/// is an array of glyph IDs rather than character codes, already shaped by the
/// caller). `advances` optionally overrides each glyph's natural advance
/// (GDI's `lpDx` array, in pixels); an empty/short slice uses natural advances
/// for the remaining glyphs. `opts` behaves as in [`rasterize`].
pub fn rasterize_glyphs(
    font_data: &[u8],
    font_index: u32,
    pixel_size: f32,
    glyph_ids: &[u16],
    advances: &[f32],
    mode: RenderMode,
    opts: RenderOptions,
) -> Option<RasterizedText> {
    if glyph_ids.is_empty() {
        return Some(RasterizedText {
            glyphs: Vec::new(),
            advance: 0.0,
            ascent: 0.0,
            descent: 0.0,
            content: mode_format(mode).1,
        });
    }

    let font = FontRef::from_index(font_data, font_index as usize)?;
    // Advances come from the font's horizontal metrics (no shaping available
    // for a bare glyph-index run), scaled to the requested pixel size. The
    // full metrics supply the ascent/descent used to place the baseline.
    let proxy = MetricsProxy::from_font(&font);
    let font_metrics = proxy.materialize_metrics(&font, &[]).scale(pixel_size);
    let glyph_metrics = proxy
        .materialize_glyph_metrics(&font, &[])
        .scale(pixel_size);

    let (format, content) = mode_format(mode);

    let mut scale_context = ScaleContext::new();
    let mut scaler = scale_context
        .builder(font)
        .size(pixel_size)
        .hint(true)
        .build();

    // Faux bold: 0..=100 maps to 0..1px of outline stroke thickening.
    let embolden = opts.boldness as f32 / 100.0;

    let mut glyphs: Vec<PlacedGlyph> = Vec::new();
    let mut pen = 0.0f32;
    let mut failed = false;

    for (i, &id) in glyph_ids.iter().enumerate() {
        let advance = if i < advances.len() {
            advances[i]
        } else {
            glyph_metrics.advance_width(id)
        };

        // Glyph-index runs carry no subpixel phase, so rasterize at the
        // integer origin; grayscale coverage still lands smoothly.
        let sources = [
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ];
        let mut render = Render::new(&sources);
        render.format(format);
        render.offset(Vector::new(0.0, 0.0));
        if embolden > 0.0 {
            render.embolden(embolden);
        }
        let image = render.render(&mut scaler, id);
        let Some(image) = image else {
            pen += advance;
            continue;
        };
        if image.placement.width == 0 || image.placement.height == 0 {
            pen += advance;
            continue;
        }
        if !matches!(image.content, Content::Mask | Content::SubpixelMask) {
            failed = true;
            break;
        }
        let bpp = match image.content {
            Content::Mask => 1,
            _ => 4,
        };
        if image.data.len() != (image.placement.width * image.placement.height * bpp) as usize {
            failed = true;
            break;
        }
        let mut data = image.data;
        if matches!(image.content, Content::SubpixelMask) {
            apply_lcd_filter(&mut data, image.placement.width, image.placement.height, opts.lcd_strength);
            apply_crosstalk(&mut data, image.placement.width, image.placement.height, opts.crosstalk);
        }
        let coverage = match mode {
            RenderMode::Crisp => data
                .into_iter()
                .map(|a| if a >= 128 { 255 } else { 0 })
                .collect(),
            _ => data,
        };
        glyphs.push(PlacedGlyph {
            x: pen,
            y: 0.0,
            bitmap_left: image.placement.left,
            bitmap_top: image.placement.top,
            width: image.placement.width,
            height: image.placement.height,
            coverage,
        });
        pen += advance;
    }

    if failed {
        return None;
    }
    Some(RasterizedText {
        glyphs,
        advance: pen,
        ascent: font_metrics.ascent,
        descent: font_metrics.descent,
        content,
    })
}

/// A flattened coverage image for a rasterized run, top-down, ready to be
/// composited (by the hook's blit or the app preview). `content` determines
/// the bytes per pixel: `Mask` = 1 byte grayscale, `SubpixelMask` = 4 bytes
/// RGBA subpixel coverage.
#[derive(Clone, Debug)]
pub struct CoverageBitmap {
    pub width: u32,
    pub height: u32,
    pub content: Content,
    /// `width * height * bpp` bytes, row-major, top-down.
    pub data: Vec<u8>,
}

/// Composite a rasterized run into a single tight coverage bitmap, using the
/// same origin convention as the hook's blit (baseline at y=0, bitmap top
/// measured upward). Overlapping glyphs merge per-pixel with max so no
/// coverage is lost. Returns None for an empty run.
pub fn composite_run(run: &RasterizedText) -> Option<CoverageBitmap> {
    if run.glyphs.is_empty() {
        return None;
    }
    let mut min_left = i32::MAX;
    let mut min_top = i32::MAX;
    let mut max_right = i32::MIN;
    let mut max_bottom = i32::MIN;
    for g in &run.glyphs {
        let left = g.x.floor() as i32 + g.bitmap_left;
        let top = g.y.floor() as i32 - g.bitmap_top;
        let right = left + g.width as i32;
        let bottom = top + g.height as i32;
        min_left = min_left.min(left);
        min_top = min_top.min(top);
        max_right = max_right.max(right);
        max_bottom = max_bottom.max(bottom);
    }
    let w = (max_right - min_left).max(1) as usize;
    let h = (max_bottom - min_top).max(1) as usize;
    let bpp = match run.content {
        Content::SubpixelMask => 4,
        _ => 1,
    };
    let mut data = vec![0u8; w * h * bpp];
    for g in &run.glyphs {
        let dst_left = g.x.floor() as i32 + g.bitmap_left - min_left;
        let dst_top = g.y.floor() as i32 - g.bitmap_top - min_top;
        for row in 0..g.height as usize {
            for col in 0..g.width as usize {
                let dx = dst_left as usize + col;
                let dy = dst_top as usize + row;
                if dx >= w || dy >= h {
                    continue;
                }
                let src = (row * g.width as usize + col) * bpp;
                let dst = (dy * w + dx) * bpp;
                for c in 0..bpp {
                    data[dst + c] = data[dst + c].max(g.coverage[src + c]);
                }
            }
        }
    }
    Some(CoverageBitmap {
        width: w as u32,
        height: h as u32,
        content: run.content,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_test_font() -> Option<Vec<u8>> {
        // A Windows-only app, so a system font is a fine test fixture. Prefer
        // Arial (ubiquitous) and fall back to Segoe UI.
        let candidates = [
            r"C:\Windows\Fonts\arial.ttf",
            r"C:\Windows\Fonts\segoeui.ttf",
        ];
        for path in candidates {
            if let Ok(data) = std::fs::read(path) {
                return Some(data);
            }
        }
        None
    }

    #[test]
    fn grayscale_rasterizes_text() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        let out =
            rasterize(&font, 0, 16.0, "Ag", RenderMode::Grayscale, RenderOptions::default())
                .expect("font parses");
        assert!(out.advance > 0.0);
        assert!(!out.glyphs.is_empty());
        assert!(out
            .glyphs
            .iter()
            .any(|g| g.coverage.iter().any(|&c| c > 0)));
    }

    #[test]
    fn crisp_thresholds_to_monochrome() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        let out =
            rasterize(&font, 0, 16.0, "Ag", RenderMode::Crisp, RenderOptions::default())
                .expect("font parses");
        assert!(!out.glyphs.is_empty());
        for g in &out.glyphs {
            assert!(g.coverage.iter().all(|&c| c == 0 || c == 255));
        }
    }

    #[test]
    fn rasterizes_glyph_indices_by_id() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        let font_ref = FontRef::from_index(&font, 0).expect("font parses");
        let id = font_ref.charmap().map('A');
        assert_ne!(id, 0);
        let out = rasterize_glyphs(
            &font,
            0,
            16.0,
            &[id],
            &[],
            RenderMode::Grayscale,
            RenderOptions::default(),
        )
        .expect("rasterizes by glyph id");
        assert!(out.advance > 0.0);
        assert!(!out.glyphs.is_empty());
        assert!(out.glyphs[0].coverage.iter().any(|&c| c > 0));
    }

    #[test]
    fn subpixel_rasterizes_all_layouts() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        for mode in [
            RenderMode::SubpixelRgb,
            RenderMode::SubpixelBgr,
            RenderMode::SubpixelRgwb,
            RenderMode::SubpixelRwbg,
        ] {
            let out = rasterize(&font, 0, 16.0, "Ag", mode, RenderOptions::default())
                .expect("font parses");
            assert!(!out.glyphs.is_empty());
            assert_eq!(out.content, Content::SubpixelMask);
            for g in &out.glyphs {
                assert_eq!(
                    g.coverage.len(),
                    (g.width * g.height * 4) as usize,
                    "subpixel coverage must be 4 bytes/pixel"
                );
                // At least one subpixel channel should carry coverage.
                assert!(g
                    .coverage
                    .chunks(4)
                    .any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0));
            }
        }
    }

    #[test]
    fn lcd_filter_spreads_impulse() {
        let mut buf = vec![0u8; 5 * 4];
        buf[2 * 4] = 255; // red-channel impulse at pixel 2
        apply_lcd_filter(&mut buf, 5, 1, 100);
        // The impulse spreads across neighboring SUBPIXELS (R/G/B boundaries),
        // not just the red channel of neighboring pixels.
        assert_eq!(buf[1 * 4 + 1], 15); // G of pixel 1
        assert_eq!(buf[1 * 4 + 2], 63); // B of pixel 1
        assert_eq!(buf[2 * 4], 111); // R of pixel 2 (peak)
        assert_eq!(buf[2 * 4 + 1], 63); // G of pixel 2
        assert_eq!(buf[2 * 4 + 2], 15); // B of pixel 2
    }

    #[test]
    fn composite_run_flattens_glyphs() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        let run = rasterize(&font, 0, 16.0, "Ag", RenderMode::Grayscale, RenderOptions::default())
            .expect("font parses");
        let bmp = composite_run(&run).expect("non-empty run");
        assert!(bmp.width > 0 && bmp.height > 0);
        assert_eq!(bmp.content, Content::Mask);
        assert_eq!(bmp.data.len(), (bmp.width * bmp.height) as usize);
        assert!(bmp.data.iter().any(|&c| c > 0));
    }

}
