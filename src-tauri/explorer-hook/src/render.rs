//! render.rs — pure-Rust glyph rasterization core for the hook DLL.
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
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::{Direction, ShapeContext};
use swash::text::Script;
use swash::zeno::{Format, Vector};
use swash::FontRef;

/// How the current mode maps to rasterization. Mode 0 (passthrough) never
/// reaches this code — the caller forwards to GDI instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderMode {
    /// Force grayscale antialiasing (Format::Alpha, 8-bit coverage).
    Grayscale,
    /// Force no antialiasing: threshold the coverage to monochrome.
    Crisp,
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
}

/// Shape and rasterize `text` with a single font at `pixel_size`.
///
/// `font_data` is the raw bytes of the font file and `font_index` selects the
/// face within a TTC. `mode` controls grayscale vs crisp output. Returns `None`
/// if the font cannot be parsed.
pub fn rasterize(
    font_data: &[u8],
    font_index: u32,
    pixel_size: f32,
    text: &str,
    mode: RenderMode,
) -> Option<RasterizedText> {
    if text.is_empty() {
        return Some(RasterizedText {
            glyphs: Vec::new(),
            advance: 0.0,
            ascent: 0.0,
            descent: 0.0,
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

    // One scale context/scaler for the whole run (same size + hinting).
    let mut scale_context = ScaleContext::new();
    let mut scaler = scale_context
        .builder(font)
        .size(pixel_size)
        .hint(true)
        .build();

    let mut glyphs: Vec<PlacedGlyph> = Vec::new();
    let mut advance = 0.0f32;
    let mut failed = false;

    shaper.shape_with(|cluster| {
        if failed {
            return;
        }
        advance += cluster.advance();
        for glyph in cluster.glyphs {
            // Fractional subpixel phase for this glyph. Grayscale still applies
            // it so coverage lands smoothly; crisp is thresholded afterward.
            let offset = Vector::new(glyph.x.fract(), glyph.y.fract());
            let image = Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ])
            .format(Format::Alpha)
            .offset(offset)
            .render(&mut scaler, glyph.id);
            let Some(image) = image else {
                continue;
            };
            if image.placement.width == 0 || image.placement.height == 0 {
                continue;
            }
            // Only accept 8-bit coverage masks. Color (BGRA) or bitmap strikes
            // use a different byte layout; bail out so the caller falls back to
            // GDI rather than misread the buffer.
            if !matches!(image.content, Content::Mask) {
                failed = true;
                return;
            }
            if image.data.len() != (image.placement.width * image.placement.height) as usize {
                failed = true;
                return;
            }
            let coverage = match mode {
                RenderMode::Grayscale => image.data,
                RenderMode::Crisp => image
                    .data
                    .into_iter()
                    .map(|a| if a >= 128 { 255 } else { 0 })
                    .collect(),
            };
            glyphs.push(PlacedGlyph {
                x: glyph.x,
                y: glyph.y,
                bitmap_left: image.placement.left,
                bitmap_top: image.placement.top,
                width: image.placement.width,
                height: image.placement.height,
                coverage,
            });
        }
    });

    if failed {
        return None;
    }
    Some(RasterizedText {
        glyphs,
        advance,
        ascent: metrics.ascent,
        descent: metrics.descent,
    })
}

/// Rasterize a run of glyph indices (GDI's `ETO_GLYPH_INDEX` path: the string
/// is an array of glyph IDs rather than character codes, already shaped by the
/// caller). `advances` optionally overrides each glyph's natural advance
/// (GDI's `lpDx` array, in pixels); an empty/short slice uses natural advances
/// for the remaining glyphs.
pub fn rasterize_glyphs(
    font_data: &[u8],
    font_index: u32,
    pixel_size: f32,
    glyph_ids: &[u16],
    advances: &[f32],
    mode: RenderMode,
) -> Option<RasterizedText> {
    if glyph_ids.is_empty() {
        return Some(RasterizedText {
            glyphs: Vec::new(),
            advance: 0.0,
            ascent: 0.0,
            descent: 0.0,
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

    let mut scale_context = ScaleContext::new();
    let mut scaler = scale_context
        .builder(font)
        .size(pixel_size)
        .hint(true)
        .build();

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
        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(Format::Alpha)
        .offset(Vector::new(0.0, 0.0))
        .render(&mut scaler, id);
        let Some(image) = image else {
            pen += advance;
            continue;
        };
        if image.placement.width == 0 || image.placement.height == 0 {
            pen += advance;
            continue;
        }
        if !matches!(image.content, Content::Mask) {
            failed = true;
            break;
        }
        if image.data.len() != (image.placement.width * image.placement.height) as usize {
            failed = true;
            break;
        }
        let coverage = match mode {
            RenderMode::Grayscale => image.data,
            RenderMode::Crisp => image
                .data
                .into_iter()
                .map(|a| if a >= 128 { 255 } else { 0 })
                .collect(),
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
        let out = rasterize(&font, 0, 16.0, "Ag", RenderMode::Grayscale).expect("font parses");
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
        let out = rasterize(&font, 0, 16.0, "Ag", RenderMode::Crisp).expect("font parses");
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
        let out = rasterize_glyphs(&font, 0, 16.0, &[id], &[], RenderMode::Grayscale)
            .expect("rasterizes by glyph id");
        assert!(out.advance > 0.0);
        assert!(!out.glyphs.is_empty());
        assert!(out.glyphs[0].coverage.iter().any(|&c| c > 0));
    }
}
