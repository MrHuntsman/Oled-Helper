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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use swash::proxy::MetricsProxy;
pub use swash::scale::image::Content;
use swash::scale::image::Image;
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
    /// Coverage gamma (text weight), 0..=100 mapped to γ 1.0..2.0. Applies a
    /// power curve to the coverage luminance while preserving per-pixel
    /// chroma ratios, so it thickens/softens the text without shifting the
    /// fringe colors. 0 = no change. Works on all modes.
    pub gamma: u8,
    /// Fringe damping, 0..=100: blends each subpixel pixel toward its
    /// luminance (c → lerp(c, lum, t)). Reduces fringe chroma so colored
    /// edges fade toward gray — the inverse of crosstalk, useful when
    /// fringing is more distracting than the color loss. 0 = no damping.
    /// Only affects subpixel modes.
    pub damping: u8,
    /// LCD filter width, 0..=100: interpolates the 5-tap FIR between
    /// FreeType's lcddefault taps (0, current behavior) and a wider, softer
    /// spread (100) that smears the fringe over more subpixels. 0 = the
    /// historical lcddefault. Only affects subpixel modes.
    pub filter_width: u8,
    /// Custom subpixel centers (R, G, B) as per-channel (x, y) fractions of
    /// a pixel, used only when `mode == RenderMode::Custom`. None = the
    /// mode's default layout. Lets the user tune a panel's physical subpixel
    /// geometry against the live preview (the WOLED quad and QD-OLED layouts
    /// are just fixed presets of the same mechanism). Horizontal-only layouts
    /// (all y = 0) reuse swash's native 1D subpixel path; any vertical
    /// component is rendered as three per-channel alpha passes shifted by
    /// that channel's (x, y).
    pub subpixel_offsets: Option<[[f32; 2]; 3]>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            lcd_strength: 100,
            crosstalk: 0,
            boldness: 0,
            gamma: 0,
            damping: 0,
            filter_width: 0,
            subpixel_offsets: None,
        }
    }
}

/// How the current mode maps to rasterization. Mode 0 (passthrough) never
/// reaches this code — the caller forwards to GDI instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RenderMode {
    /// Force grayscale antialiasing (Format::Alpha, 8-bit coverage).
    Grayscale,
    /// Force no antialiasing: threshold the coverage to monochrome.
    Crisp,
    /// Subpixel antialiasing for an RGB panel (per-channel coverage).
    SubpixelRgb,
    /// Subpixel antialiasing for a BGR panel (per-channel coverage).
    SubpixelBgr,
    /// User-tuned subpixel centers from `RenderOptions.subpixel_offsets`
    /// (falls back to the WOLED quad positions when unset). The live
    /// calibration mode: pick the offsets that make the fringing match the
    /// panel.
    Custom,
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
    /// 8-bit coverage, `width * height` bytes, row-major. Shared with the
    /// glyph cache via `Arc` — the blit only reads it, so the run and the
    /// cache share one immutable buffer instead of copying it per draw.
    pub coverage: Arc<[u8]>,
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
        // Custom: the WOLED quad positions as a safe default when no offsets
        // were supplied; rasterize()/rasterize_glyphs() override the format
        // with the user's offsets before rendering.
        RenderMode::Custom => (
            Format::CustomSubpixel([-0.375, -0.125, 0.375]),
            Content::SubpixelMask,
        ),
    }
}

/// The swash format for a 1D render: the mode's layout, or the user's custom
/// x-centers when mode is Custom and offsets are present. Layouts with any
/// vertical component never reach here — `custom_2d_offsets` routes them to
/// the three-pass path — so only the x components are the native centers.
fn render_format(mode: RenderMode, opts: RenderOptions) -> Format {
    let (format, _) = mode_format(mode);
    if mode == RenderMode::Custom {
        if let Some(offsets) = opts.subpixel_offsets {
            return Format::CustomSubpixel([offsets[0][0], offsets[1][0], offsets[2][0]]);
        }
    }
    format
}

/// The effective custom subpixel centers when a render must use the 2D
/// three-pass path: the mode is Custom, offsets are supplied, and at least
/// one channel has a vertical component (swash's native subpixel rasterizer
/// only supports per-channel x offsets).
fn custom_2d_offsets(mode: RenderMode, opts: RenderOptions) -> Option<[[f32; 2]; 3]> {
    if mode != RenderMode::Custom {
        return None;
    }
    let offsets = opts.subpixel_offsets?;
    if offsets.iter().any(|[_, y]| *y != 0.0) {
        Some(offsets)
    } else {
        None
    }
}

/// Subpixel phase quantum: 1/16 of a pixel. The character path's pen position
/// carries an arbitrary fractional phase; snapping it to sixteenths collapses
/// that continuum into 16 buckets (imperceptible at render time) so repeated
/// draws at nearby x positions share glyph-cache entries instead of
/// re-rasterizing a slightly different phase every time.
const PHASE_QUANTUM: f32 = 1.0 / 16.0;

/// Quantize a subpixel phase to [`PHASE_QUANTUM`], wrapping a value that
/// rounds up to exactly 1.0 back to 0 so the phase stays in [0, 1) and the
/// cache key is canonical.
fn quantize_phase(v: f32) -> f32 {
    let snapped = (v / PHASE_QUANTUM).round() * PHASE_QUANTUM;
    if snapped >= 1.0 {
        snapped - 1.0
    } else {
        snapped
    }
}

/// FreeType's `lcddefault` filter weights (sum 272, slightly over-unity to
/// compensate the low-pass energy loss, matching FreeType).
const LCD_FILTER_FULL: [f32; 5] = [16.0, 64.0, 112.0, 64.0, 16.0];
/// The identity filter (strength 0): passes coverage through unchanged.
const LCD_FILTER_IDENTITY: [f32; 5] = [0.0, 0.0, 256.0, 0.0, 0.0];
/// A wider, softer spread (sum 272, same energy as lcddefault): pushes more
/// weight onto the outer taps so the fringe smears across more subpixels.
/// The width knob interpolates between lcddefault (0) and this (100).
const LCD_FILTER_WIDE: [f32; 5] = [32.0, 80.0, 96.0, 80.0, 32.0];

/// FreeType's `lcddefault` filter: a 5-tap horizontal FIR applied to the
/// subpixel coverage stream (R, G, B interleaved), dividing by 256. The
/// weights sum to 272, so the slight over-unity gain compensates the low-pass
/// energy loss (matching FreeType). Because each tap advances one SUBPIXEL
/// rather than one pixel, the filter spans roughly 1.7 pixels and only softens
/// the color fringing at glyph edges instead of blurring the glyph shape. Edges
/// are zero-padded.
///
/// `strength` (0..=100) interpolates between the identity filter (raw
/// coverage, harsher fringing) and the base filter, so it doubles as the
/// filter's on/off. `width` (0..=100) picks the base filter between the
/// historical lcddefault taps (0) and a wider, softer spread (100).
fn apply_lcd_filter(coverage: &mut [u8], width: u32, height: u32, strength: u8, filter_width: u8) {
    if strength == 0 {
        return;
    }
    let t = strength as f32 / 100.0;
    let w = filter_width as f32 / 100.0;
    let base: [f32; 5] = std::array::from_fn(|i| {
        LCD_FILTER_FULL[i] * (1.0 - w) + LCD_FILTER_WIDE[i] * w
    });
    let weights: [u32; 5] = std::array::from_fn(|i| {
        (base[i] * t + LCD_FILTER_IDENTITY[i] * (1.0 - t)).round() as u32
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

/// Coverage gamma (text weight): applies a power curve to each pixel's
/// luminance while preserving chroma ratios (all channels scale by the same
/// factor), so the fringe colors stay put and only the ink weight changes.
/// `strength` 0..=100 maps to γ 1.0..2.0 (0 = unchanged).
fn apply_gamma(coverage: &mut [u8], width: u32, height: u32, strength: u8) {
    if strength == 0 {
        return;
    }
    let gamma = 1.0 + strength as f32 / 100.0; // 1.0..2.0
    let n = width as usize * height as usize;
    for i in 0..n {
        let p = i * 4;
        let r = coverage[p] as f32;
        let g = coverage[p + 1] as f32;
        let b = coverage[p + 2] as f32;
        let lum = (r + g + b) / 3.0;
        if lum <= 0.0 {
            continue;
        }
        // scale = lum^(γ-1) makes new_lum = lum^γ (in 0..1 space). Applying
        // the same scale to all three channels preserves their ratios, so
        // hue/saturation of the fringe is untouched.
        let scale = (lum / 255.0).powf(gamma - 1.0);
        coverage[p] = (r * scale).clamp(0.0, 255.0) as u8;
        coverage[p + 1] = (g * scale).clamp(0.0, 255.0) as u8;
        coverage[p + 2] = (b * scale).clamp(0.0, 255.0) as u8;
    }
}

/// Fringe damping: blends each subpixel pixel toward its luminance so the
/// colored fringe fades toward gray. `strength` 0..=100 is the blend factor
/// (0 = untouched, 100 = fully gray fringe). Luminance-preserving by
/// construction (the target is the pixel's own mean).
fn apply_damping(coverage: &mut [u8], width: u32, height: u32, strength: u8) {
    if strength == 0 {
        return;
    }
    let t = strength as f32 / 100.0;
    let n = width as usize * height as usize;
    for i in 0..n {
        let p = i * 4;
        let r = coverage[p] as f32;
        let g = coverage[p + 1] as f32;
        let b = coverage[p + 2] as f32;
        let lum = (r + g + b) / 3.0;
        coverage[p] = (r + (lum - r) * t).round().clamp(0.0, 255.0) as u8;
        coverage[p + 1] = (g + (lum - g) * t).round().clamp(0.0, 255.0) as u8;
        coverage[p + 2] = (b + (lum - b) * t).round().clamp(0.0, 255.0) as u8;
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

// ── Per-glyph coverage cache ─────────────────────────────────────────
// Explorer repaints the same text (filenames, toolbar labels) many times per
// second while resizing or hovering, and the swash rasterization on every
// draw is the dominant cost. Cache the FINAL coverage bitmap per
// (font, size, glyph, phase, mode, knobs) and memcpy it out on hit. Bounded
// by a byte budget + entry count with FIFO eviction.

const GLYPH_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const GLYPH_CACHE_MAX_ENTRIES: usize = 8192;
const GLYPH_ENTRY_OVERHEAD: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    /// `font_data.as_ptr()` — stable because every caller (the hook's
    /// `cached_font` and the preview's `preview_font`) keeps the font bytes in
    /// a process-lifetime cache, so the pointer identifies the face.
    font: usize,
    font_index: u32,
    /// `pixel_size.to_bits()` — exact f32 identity, deterministic per caller.
    size: u32,
    glyph: u16,
    /// Subpixel phase bits (`offset.x/y.to_bits()`). The glyph-index path
    /// always uses (0,0), so those draws hit regardless of x position.
    phase_x: u32,
    phase_y: u32,
    mode: u8,
    lcd: u8,
    crosstalk: u8,
    boldness: u8,
    gamma: u8,
    damping: u8,
    filter_width: u8,
    /// Custom subpixel center bits (0 when unused) — tuned offsets must not
    /// share cache entries with the fixed layouts. All six (x, y) offsets are
    /// carried so a 2D layout never collides with a horizontal-only one.
    sub_rx: u32,
    sub_ry: u32,
    sub_gx: u32,
    sub_gy: u32,
    sub_bx: u32,
    sub_by: u32,
}

/// The rasterized bitmap for one glyph (position-independent).
#[derive(Clone)]
struct CachedGlyph {
    bitmap_left: i32,
    bitmap_top: i32,
    width: u32,
    height: u32,
    coverage: Arc<[u8]>,
}

struct GlyphCache {
    map: HashMap<GlyphCacheKey, Arc<CachedGlyph>>,
    /// Insertion order for FIFO eviction.
    order: VecDeque<GlyphCacheKey>,
    bytes: usize,
}

static GLYPH_CACHE: OnceLock<Mutex<GlyphCache>> = OnceLock::new();
// Hit/miss counters for the hook's verbose log (see glyph_cache_stats).
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

fn glyph_cache() -> &'static Mutex<GlyphCache> {
    GLYPH_CACHE.get_or_init(|| {
        Mutex::new(GlyphCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        })
    })
}

/// Cache lookup; counts hits. Hands out an `Arc` (a refcount bump — no
/// coverage copy), which the caller reads immediately and drops.
fn cache_get(key: &GlyphCacheKey) -> Option<Arc<CachedGlyph>> {
    let cache = glyph_cache().lock().unwrap_or_else(|p| p.into_inner());
    match cache.map.get(key) {
        Some(g) => {
            CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            Some(g.clone())
        }
        None => {
            CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Insert, evicting FIFO while over the budget.
fn cache_insert(key: GlyphCacheKey, glyph: Arc<CachedGlyph>) {
    let mut cache = glyph_cache().lock().unwrap_or_else(|p| p.into_inner());
    let cost = glyph.coverage.len() + GLYPH_ENTRY_OVERHEAD;
    while !cache.order.is_empty()
        && (cache.bytes + cost > GLYPH_CACHE_MAX_BYTES
            || cache.map.len() >= GLYPH_CACHE_MAX_ENTRIES)
    {
        if let Some(old_key) = cache.order.pop_front() {
            if let Some(old) = cache.map.remove(&old_key) {
                cache.bytes = cache.bytes.saturating_sub(old.coverage.len() + GLYPH_ENTRY_OVERHEAD);
            }
        }
    }
    cache.order.push_back(key);
    cache.map.insert(key, glyph);
    cache.bytes += cost;
}

/// Cumulative cache stats (hits, misses, live entries) for diagnostics.
pub fn glyph_cache_stats() -> (u64, u64, usize) {
    let entries = glyph_cache()
        .lock()
        .map(|c| c.map.len())
        .unwrap_or(0);
    (
        CACHE_HITS.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
        entries,
    )
}

/// Batch lookup: lock the cache ONCE and return the cached glyph for every
/// key (None for misses). A fully cached run is one lock acquisition instead
/// of one per glyph, which matters for Explorer's repaints (a 79-glyph run
/// redrawn every frame).
fn cache_get_batch(keys: &[GlyphCacheKey]) -> Vec<Option<Arc<CachedGlyph>>> {
    let cache = glyph_cache().lock().unwrap_or_else(|p| p.into_inner());
    keys.iter()
        .map(|k| match cache.map.get(k) {
            Some(g) => {
                CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                Some(g.clone())
            }
            None => {
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
                None
            }
        })
        .collect()
}

/// The cache key for one glyph. Everything that changes the output bitmap is
/// in the key: font identity (stable pointer), size, glyph id, subpixel
/// phase, mode, and the render knobs.
fn glyph_cache_key(
    font_ptr: usize,
    font_index: u32,
    pixel_size: f32,
    glyph_id: u16,
    offset: Vector,
    mode: RenderMode,
    opts: RenderOptions,
) -> GlyphCacheKey {
    let (sub_rx, sub_ry, sub_gx, sub_gy, sub_bx, sub_by) = match opts.subpixel_offsets {
        Some([[rx, ry], [gx, gy], [bx, by]]) => {
            (rx.to_bits(), ry.to_bits(), gx.to_bits(), gy.to_bits(), bx.to_bits(), by.to_bits())
        }
        None => (0, 0, 0, 0, 0, 0),
    };
    GlyphCacheKey {
        font: font_ptr,
        font_index,
        size: pixel_size.to_bits(),
        glyph: glyph_id,
        phase_x: offset.x.to_bits(),
        phase_y: offset.y.to_bits(),
        mode: mode as u8,
        lcd: opts.lcd_strength,
        crosstalk: opts.crosstalk,
        boldness: opts.boldness,
        gamma: opts.gamma,
        damping: opts.damping,
        filter_width: opts.filter_width,
        sub_rx,
        sub_ry,
        sub_gx,
        sub_gy,
        sub_bx,
        sub_by,
    }
}

/// Per-glyph outcome: rendered bitmap (shared with the cache), skip (empty
/// glyph), or unsupported content (bail the whole run so the caller falls
/// back to GDI).
enum GlyphResult {
    Rendered(Arc<CachedGlyph>),
    Skip,
    Unsupported,
}

/// Rasterize one glyph (or reuse the cached bitmap), applying the LCD filter,
/// crosstalk, and crisp threshold so the cache stores the FINAL coverage.
/// The caller supplies the run-local offset (subpixel phase).
fn rasterize_glyph_cached(
    scaler: &mut swash::scale::Scaler,
    font_ptr: usize,
    font_index: u32,
    pixel_size: f32,
    glyph_id: u16,
    offset: Vector,
    format: Format,
    embolden: f32,
    mode: RenderMode,
    opts: RenderOptions,
) -> GlyphResult {
    let key = glyph_cache_key(font_ptr, font_index, pixel_size, glyph_id, offset, mode, opts);
    if let Some(cached) = cache_get(&key) {
        return GlyphResult::Rendered(cached);
    }
    rasterize_glyph_uncached(scaler, key, glyph_id, offset, format, embolden, mode, opts)
}

/// Render a glyph without touching the cache, then insert the result. Used by
/// [`rasterize_glyph_cached`] and by the glyph-index path after a batch
/// lookup already confirmed the miss.
fn rasterize_glyph_uncached(
    scaler: &mut swash::scale::Scaler,
    key: GlyphCacheKey,
    glyph_id: u16,
    offset: Vector,
    format: Format,
    embolden: f32,
    mode: RenderMode,
    opts: RenderOptions,
) -> GlyphResult {
    // A custom layout with any vertical component cannot use swash's native
    // subpixel rasterizer (its offsets are x-only): render three per-channel
    // alpha passes instead and combine them into one RGBA buffer.
    if let Some(offsets) = custom_2d_offsets(mode, opts) {
        return rasterize_glyph_2d(scaler, key, glyph_id, offset, embolden, offsets, opts);
    }
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
    let image = render.render(scaler, glyph_id);
    let Some(image) = image else {
        return GlyphResult::Skip;
    };
    if image.placement.width == 0 || image.placement.height == 0 {
        return GlyphResult::Skip;
    }
    // Only accept the coverage formats we know how to composite. Color (BGRA)
    // or bitmap strikes use a different byte layout; bail out so the caller
    // falls back to GDI rather than misread the buffer.
    if !matches!(image.content, Content::Mask | Content::SubpixelMask) {
        return GlyphResult::Unsupported;
    }
    let bpp = match image.content {
        Content::Mask => 1,
        _ => 4,
    };
    if image.data.len() != (image.placement.width * image.placement.height * bpp) as usize {
        return GlyphResult::Unsupported;
    }
    let mut data = image.data;
    if matches!(image.content, Content::SubpixelMask) {
        postprocess_subpixel(&mut data, image.placement.width, image.placement.height, opts);
    }
    let glyph = finish_glyph(key, image.placement.left, image.placement.top, image.placement.width, image.placement.height, data, mode);
    GlyphResult::Rendered(glyph)
}

/// The subpixel post-processing knobs shared by the native and 2D paths. All
/// four operate on a 4-byte-per-pixel RGBA buffer and only affect subpixel
/// coverage (callers gate on `Content::SubpixelMask`).
fn postprocess_subpixel(data: &mut [u8], width: u32, height: u32, opts: RenderOptions) {
    apply_lcd_filter(data, width, height, opts.lcd_strength, opts.filter_width);
    apply_crosstalk(data, width, height, opts.crosstalk);
    apply_gamma(data, width, height, opts.gamma);
    apply_damping(data, width, height, opts.damping);
}

/// Finalize a rendered glyph: apply the crisp threshold when the mode is
/// Crisp, then wrap it into a cache entry and store it.
fn finish_glyph(
    key: GlyphCacheKey,
    bitmap_left: i32,
    bitmap_top: i32,
    width: u32,
    height: u32,
    data: Vec<u8>,
    mode: RenderMode,
) -> Arc<CachedGlyph> {
    let coverage: Arc<[u8]> = match mode {
        RenderMode::Crisp => data
            .into_iter()
            .map(|a| if a >= 128 { 255 } else { 0 })
            .collect::<Vec<_>>()
            .into(),
        _ => data.into(),
    };
    let glyph = Arc::new(CachedGlyph {
        bitmap_left,
        bitmap_top,
        width,
        height,
        coverage,
    });
    cache_insert(key, glyph.clone());
    glyph
}

/// Render a custom 2D subpixel glyph: three per-channel alpha passes, each
/// shifted by that channel's (x, y) center, combined into one 4-byte-per-pixel
/// RGBA buffer. The three placements are unioned so the per-channel vertical
/// shifts can enlarge the bitmap by a pixel. Color/bitmap strikes cannot be
/// split into channel passes, so they bail (the caller falls back to GDI).
fn rasterize_glyph_2d(
    scaler: &mut swash::scale::Scaler,
    key: GlyphCacheKey,
    glyph_id: u16,
    phase: Vector,
    embolden: f32,
    offsets: [[f32; 2]; 3],
    opts: RenderOptions,
) -> GlyphResult {
    let sources = [
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ];
    let mut passes: Vec<Image> = Vec::with_capacity(3);
    for [cx, cy] in offsets {
        let mut render = Render::new(&sources);
        render.format(Format::Alpha);
        render.offset(Vector::new(phase.x + cx, phase.y + cy));
        if embolden > 0.0 {
            render.embolden(embolden);
        }
        let Some(image) = render.render(scaler, glyph_id) else {
            return GlyphResult::Skip;
        };
        if image.content != Content::Mask {
            // Color/bitmap content has no per-channel alpha to split.
            return GlyphResult::Unsupported;
        }
        if image.placement.width == 0 || image.placement.height == 0 {
            // Empty glyph (space/separator): draw nothing, like the native path.
            return GlyphResult::Skip;
        }
        passes.push(image);
    }
    // Union the three placements (each vertical shift can move a pass's top
    // by a pixel) into one tight buffer.
    let min_left = passes.iter().map(|p| p.placement.left).min().unwrap();
    let min_top = passes.iter().map(|p| p.placement.top).min().unwrap();
    let max_right = passes
        .iter()
        .map(|p| p.placement.left + p.placement.width as i32)
        .max()
        .unwrap();
    let max_bottom = passes
        .iter()
        .map(|p| p.placement.top + p.placement.height as i32)
        .max()
        .unwrap();
    let width = (max_right - min_left).max(1) as u32;
    let height = (max_bottom - min_top).max(1) as u32;
    let mut data = vec![0u8; (width * height * 4) as usize];
    for (c, pass) in passes.iter().enumerate() {
        let dx = (pass.placement.left - min_left) as usize;
        let dy = (pass.placement.top - min_top) as usize;
        for row in 0..pass.placement.height as usize {
            let src_row = row * pass.placement.width as usize;
            let dst_row = (dy + row) * width as usize;
            for col in 0..pass.placement.width as usize {
                let cov = pass.data[src_row + col];
                if cov != 0 {
                    data[(dst_row + dx + col) * 4 + c] = cov;
                }
            }
        }
    }
    postprocess_subpixel(&mut data, width, height, opts);
    let glyph = finish_glyph(key, min_left, min_top, width, height, data, RenderMode::Custom);
    GlyphResult::Rendered(glyph)
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

    // Whitespace-only runs carry no ink. Returning an empty run here skips
    // the shaper + scaler build entirely — spaces and separators are drawn
    // constantly by Explorer (each as its own count=1 ExtTextOutW) and would
    // otherwise pay the full swash setup for a glyph that has no outline. The
    // advance is taken from the font's hmtx so right/center-aligned draws
    // keep their reference point (it is unused by the blit for empty runs).
    if text.chars().all(|c| c.is_whitespace()) {
        let proxy = MetricsProxy::from_font(&font);
        let glyph_metrics = proxy.materialize_glyph_metrics(&font, &[]).scale(pixel_size);
        let advance = text
            .chars()
            .map(|c| glyph_metrics.advance_width(font.charmap().map(c)))
            .sum::<f32>();
        return Some(RasterizedText {
            glyphs: Vec::new(),
            advance,
            ascent: metrics.ascent,
            descent: metrics.descent,
            content: mode_format(mode).1,
        });
    }

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

    let (_, content) = mode_format(mode);
    // Custom mode renders with the user's subpixel centers (see
    // render_format); the fixed modes use their preset layouts.
    let format = render_format(mode, opts);

    // One scale context/scaler for the whole run (same size + hinting).
    let mut scale_context = ScaleContext::new();
    let mut scaler = scale_context
        .builder(font)
        .size(pixel_size)
        .hint(true)
        .build();

    // Faux bold: 0..=100 maps to 0..1px of outline stroke thickening.
    let embolden = opts.boldness as f32 / 100.0;

    // Font bytes identity for the glyph cache: the caller keeps the Arc alive
    // for the process lifetime, so the pointer is stable across draws.
    let font_ptr = font_data.as_ptr() as usize;

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
            // Fractional subpixel phase for this glyph, quantized to 1/16 px so
            // nearby draws hit the cache. Grayscale still applies it so
            // coverage lands smoothly; crisp is thresholded afterward.
            let offset = Vector::new(quantize_phase(gx.fract()), quantize_phase(glyph.y.fract()));
            match rasterize_glyph_cached(
                &mut scaler,
                font_ptr,
                font_index,
                pixel_size,
                glyph.id,
                offset,
                format,
                embolden,
                mode,
                opts,
            ) {
                GlyphResult::Rendered(c) => {
                    glyphs.push(PlacedGlyph {
                        x: gx,
                        y: glyph.y,
                        bitmap_left: c.bitmap_left,
                        bitmap_top: c.bitmap_top,
                        width: c.width,
                        height: c.height,
                        coverage: c.coverage.clone(),
                    });
                    pen += glyph.advance;
                }
                // Empty glyph: draw nothing, keep the pen (matches prior
                // behavior of skipping the rasterization).
                GlyphResult::Skip => {}
                // Color/bitmap content we cannot composite: fail the run so
                // the caller falls back to GDI untouched.
                GlyphResult::Unsupported => {
                    failed = true;
                    return;
                }
            }
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

    let (_, content) = mode_format(mode);
    // Custom mode renders with the user's subpixel centers (see
    // render_format); the fixed modes use their preset layouts.
    let format = render_format(mode, opts);

    // Faux bold: 0..=100 maps to 0..1px of outline stroke thickening.
    let embolden = opts.boldness as f32 / 100.0;

    // Font bytes identity for the glyph cache: the caller keeps the Arc alive
    // for the process lifetime, so the pointer is stable across draws.
    let font_ptr = font_data.as_ptr() as usize;

    // Glyph-index runs carry no subpixel phase (rasterize at the integer
    // origin), so every glyph's key is known up front. Look the whole run up
    // in one lock acquisition: when every glyph is cached — the common
    // repaint case while resizing, where only the position changes — the
    // swash scaler is never built.
    let keys: Vec<GlyphCacheKey> = glyph_ids
        .iter()
        .map(|&id| {
            glyph_cache_key(font_ptr, font_index, pixel_size, id, Vector::new(0.0, 0.0), mode, opts)
        })
        .collect();
    let mut cached = cache_get_batch(&keys);
    let all_hit = cached.iter().all(Option::is_some);
    // The scaler borrows its scale context, so the context must live in the
    // same scope; both are only built when the run has any cache miss.
    let mut scale_context = if all_hit {
        None
    } else {
        Some(ScaleContext::new())
    };
    let mut scaler = scale_context
        .as_mut()
        .map(|sc| sc.builder(font).size(pixel_size).hint(true).build());

    let mut glyphs: Vec<PlacedGlyph> = Vec::new();
    let mut pen = 0.0f32;
    let mut failed = false;

    for (i, &id) in glyph_ids.iter().enumerate() {
        let advance = if i < advances.len() {
            advances[i]
        } else {
            glyph_metrics.advance_width(id)
        };

        if let Some(c) = cached[i].take() {
            glyphs.push(PlacedGlyph {
                x: pen,
                y: 0.0,
                bitmap_left: c.bitmap_left,
                bitmap_top: c.bitmap_top,
                width: c.width,
                height: c.height,
                coverage: c.coverage.clone(),
            });
            pen += advance;
            continue;
        }

        // Cache miss: rasterize now. The scaler is built lazily, so fully
        // cached repaints never pay for it.
        let Some(scaler) = scaler.as_mut() else {
            failed = true;
            break;
        };
        match rasterize_glyph_uncached(
            scaler,
            keys[i],
            id,
            Vector::new(0.0, 0.0),
            format,
            embolden,
            mode,
            opts,
        ) {
            GlyphResult::Rendered(c) => {
                glyphs.push(PlacedGlyph {
                    x: pen,
                    y: 0.0,
                    bitmap_left: c.bitmap_left,
                    bitmap_top: c.bitmap_top,
                    width: c.width,
                    height: c.height,
                    coverage: c.coverage.clone(),
                });
                pen += advance;
            }
            // Empty glyph: draw nothing but still advance (matches prior
            // behavior of advancing past un-rasterized glyphs).
            GlyphResult::Skip => {
                pen += advance;
            }
            GlyphResult::Unsupported => {
                failed = true;
                break;
            }
        }
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
    fn custom_subpixel_offsets_are_honored() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        // Custom mode with explicit offsets must rasterize as 4bpp subpixel
        // and produce a different fringe than the default WOLED quad layout.
        let default = rasterize(&font, 0, 16.0, "A", RenderMode::Custom, RenderOptions::default())
            .expect("font parses");
        let tuned = rasterize(
            &font,
            0,
            16.0,
            "A",
            RenderMode::Custom,
            RenderOptions {
                subpixel_offsets: Some([[-0.5, 0.0], [0.0, 0.0], [0.5, 0.0]]),
                ..RenderOptions::default()
            },
        )
        .expect("font parses");
        assert_eq!(default.content, Content::SubpixelMask);
        assert_eq!(tuned.content, Content::SubpixelMask);
        assert!(!tuned.glyphs.is_empty());
        let a = &tuned.glyphs[0].coverage;
        let b = &default.glyphs[0].coverage;
        assert!(a.len() == b.len(), "same glyph size at same px");
        assert!(a != b, "different offsets must change the fringe");
    }

    #[test]
    fn custom_subpixel_2d_offsets_are_honored() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        // A vertical component forces the three-pass path instead of swash's
        // native 1D subpixel rasterization (which only supports per-channel x).
        // "A B" also exercises the empty-glyph (space) path so a space can't
        // abort the whole run.
        let flat = rasterize(
            &font,
            0,
            16.0,
            "A B",
            RenderMode::Custom,
            RenderOptions {
                subpixel_offsets: Some([[-0.375, 0.0], [-0.125, 0.0], [0.375, 0.0]]),
                ..RenderOptions::default()
            },
        )
        .expect("font parses");
        let shifted = rasterize(
            &font,
            0,
            16.0,
            "A B",
            RenderMode::Custom,
            RenderOptions {
                subpixel_offsets: Some([[-0.375, 0.25], [-0.125, 0.0], [0.375, -0.25]]),
                ..RenderOptions::default()
            },
        )
        .expect("font parses");
        assert_eq!(shifted.content, Content::SubpixelMask);
        assert_eq!(shifted.glyphs.len(), 2, "the space glyph must be skipped");
        let g = &shifted.glyphs[0];
        assert_eq!(
            g.coverage.len(),
            (g.width * g.height * 4) as usize,
            "2D subpixel coverage must be 4 bytes/pixel"
        );
        assert!(
            g.coverage.chunks(4).any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0),
            "2D subpixel coverage must carry ink"
        );
        // The vertical shift must change the fringe versus the flat x-only
        // layout; if y were ignored the two would rasterize identically.
        assert_ne!(
            flat.glyphs[0].coverage, shifted.glyphs[0].coverage,
            "a vertical offset must change the coverage"
        );
    }

    #[test]
    fn phase_quantization_snaps_to_sixteenths() {
        // Sixteenth-of-a-pixel buckets: nearby phases collapse to the same
        // bucket so repeated draws at slightly different x positions share a
        // glyph-cache entry.
        assert_eq!(quantize_phase(0.0), 0.0);
        assert_eq!(quantize_phase(0.06), 0.0625);
        assert_eq!(quantize_phase(0.07), 0.0625);
        assert_eq!(quantize_phase(0.10), 0.125);
        assert_eq!(quantize_phase(0.5), 0.5);
        assert_eq!(quantize_phase(0.9375), 0.9375);
        // A phase just below 1.0 rounds up and wraps back to 0 (canonical key).
        assert_eq!(quantize_phase(0.98), 0.0);
        assert_eq!(quantize_phase(1.0), 0.0);
        // Every output is a multiple of 1/16.
        for i in 0..64 {
            let q = quantize_phase(i as f32 / 64.0);
            let k = (q * 16.0).round();
            assert!((q * 16.0 - k).abs() < 1e-5, "phase not a sixteenth: {q}");
        }
    }

    #[test]
    fn subpixel_rasterizes_all_layouts() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        for mode in [RenderMode::SubpixelRgb, RenderMode::SubpixelBgr] {
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
        apply_lcd_filter(&mut buf, 5, 1, 100, 0);
        // The impulse spreads across neighboring SUBPIXELS (R/G/B boundaries),
        // not just the red channel of neighboring pixels.
        assert_eq!(buf[1 * 4 + 1], 15); // G of pixel 1
        assert_eq!(buf[1 * 4 + 2], 63); // B of pixel 1
        assert_eq!(buf[2 * 4], 111); // R of pixel 2 (peak)
        assert_eq!(buf[2 * 4 + 1], 63); // G of pixel 2
        assert_eq!(buf[2 * 4 + 2], 15); // B of pixel 2
    }

    #[test]
    fn wide_filter_smears_farther_than_default() {
        // Same impulse, but the wide filter pushes more weight onto the outer
        // taps, so the neighbor subpixels receive more energy than lcddefault.
        let mut buf = vec![0u8; 5 * 4];
        buf[2 * 4] = 255;
        apply_lcd_filter(&mut buf, 5, 1, 100, 100);
        assert!(buf[1 * 4 + 1] > 15, "wide filter should spread more to neighbors");
        assert!(buf[2 * 4] < 111, "wide filter should lower the peak");
    }

    #[test]
    fn gamma_darkens_mid_tone_keeps_fringe_ratio() {
        // A mid-gray fringe pixel: gamma raises contrast (darkens below 50%,
        // lightens above) while preserving the R:G:B ratio so the fringe hue
        // stays put.
        let mut buf = vec![0u8; 1 * 4];
        buf[0] = 128;
        buf[1] = 64;
        buf[2] = 32;
        let before = buf.clone();
        apply_gamma(&mut buf, 1, 1, 100);
        assert!(buf[0] < before[0], "gamma should darken a sub-mid luminance");
        let r = buf[0] as f32 / before[0] as f32;
        let g = buf[1] as f32 / before[1] as f32;
        let b = buf[2] as f32 / before[2] as f32;
        let max_diff = (r - g).abs().max((r - b).abs()).max((g - b).abs());
        assert!(max_diff < 0.05, "channel ratios should be preserved");
    }

    #[test]
    fn damping_pulls_fringe_toward_gray() {
        // A pure red fringe pixel: damping blends it toward its own luminance
        // (r/3), so G and B rise toward r/3 and the chroma drops.
        let mut buf = vec![0u8; 1 * 4];
        buf[0] = 255;
        apply_damping(&mut buf, 1, 1, 100);
        assert_eq!(buf[0], 85);
        assert_eq!(buf[1], 85);
        assert_eq!(buf[2], 85);
    }

    #[test]
    fn whitespace_short_circuits_without_shaping() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        // Whitespace-only text must produce an empty run with the font's
        // advance (used by right/center alignment) and no glyphs. The fast
        // path returns before any shaper/scaler/cache work, which the code
        // structure guarantees; the stats counters are global across the
        // parallel test binary, so they are not asserted here.
        let out = rasterize(
            &font,
            0,
            16.0,
            "   ",
            RenderMode::Grayscale,
            RenderOptions::default(),
        )
        .expect("font parses");
        assert!(out.glyphs.is_empty());
        assert!(out.advance > 0.0, "space advance should be non-zero");
    }

    #[test]
    fn glyph_cache_hits_on_repeat() {
        let Some(font) = load_test_font() else {
            eprintln!("no system font available; skipping");
            return;
        };
        // First render fills the cache; the second (identical) render should
        // hit it for every glyph. Counts are cumulative across tests, so
        // compare the delta rather than absolute values.
        let before = super::glyph_cache_stats();
        let _ = rasterize(
            &font,
            0,
            16.0,
            "Ag",
            RenderMode::Grayscale,
            RenderOptions::default(),
        );
        let after_first = super::glyph_cache_stats();
        let _ = rasterize(
            &font,
            0,
            16.0,
            "Ag",
            RenderMode::Grayscale,
            RenderOptions::default(),
        );
        let after_second = super::glyph_cache_stats();
        assert!(
            after_first.1 > before.1,
            "first render should have missed at least once"
        );
        assert!(
            after_second.0 > after_first.0,
            "second identical render should hit the cache"
        );
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
