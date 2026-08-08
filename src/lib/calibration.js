// ── PQ / SDR calibration math, ported from hdr_calibration.rs ──────

export const MAX_SQUARES = 24;

export const ALL_PQ_CODES = [
  64, 68, 72, 76, 80, 84, 88, 92, 96, 100, 104, 108,
  112, 116, 120, 124, 128, 132, 136, 140, 144, 148, 152, 156,
];

export const ALL_SDR_CODES = [
  0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
  12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];

export const GAP_FRACTION = 0.008;
// High Nits is a spreadsheet-style grid rather than a single row of PQ/RGB
// columns; the same 0.008 gap used for the black strip made the grid look
// like it had visible seams between cells, so it renders edge-to-edge.
export const HIGH_NITS_GAP_FRACTION = 0;

// ── High Nits strip ─────────────────────────────────────────────────
// A second calibration strip shown as a spreadsheet-style grid of solid
// cells (rows × columns), each cell a different value. Reading order is
// left→right, top→bottom, from lower to higher nits. In SDR it shows a
// bright-RGB ramp instead.

// The High Nits grid is a FIXED 3 rows × 7 columns (21 cells) — no zoom.
// The user picks the minimum nits and the step between neighboring cells
// (granularity); the maximum derives as min + step × 20. Only two of the
// three are independent, and stores.svelte.js keeps them in sync.
export const HIGH_NITS_COLS = 7;
export const HIGH_NITS_ROWS = 3;
export const HIGH_NITS_CELLS = HIGH_NITS_COLS * HIGH_NITS_ROWS; // 21
export const HIGH_NITS_DEFAULT_MIN = 100;
// Max slider stops — the ONLY positions the Max slider rests on: 100-step
// from 400 to 1000, 500-step from 1000 to 5000, then 1000-step to the
// 10000 ceiling. Every stop is a value in the region-stepped ladder, so the
// grid can always end exactly on it. Stops start at 400 (not 100) because a
// 21-cell grid can't honor the minimum floor (HIGH_NITS_GRID_MIN_FLOOR = 300)
// for any Max below 400.
export const HIGH_NITS_MAX_STOPS = (() => {
  const stops = [];
  for (let v = 400; v <= 1000; v += 100) stops.push(v);
  for (let v = 1500; v <= 5000; v += 500) stops.push(v);
  for (let v = 6000; v <= 10000; v += 1000) stops.push(v);
  return stops;
})();
export const HIGH_NITS_DEFAULT_MAX = 2000;
// Step readout for the default Max follows the region rule (see
// highNitsStepForMax): 2000 falls in the 1000–5000 band → 500.
export const HIGH_NITS_DEFAULT_STEP = 500;
// Nits above the HDR spec ceiling are meaningless, so every control clamps
// so the derived max never exceeds it.
export const HIGH_NITS_MAX_CEILING = 10000;

// Round steps used to fill the grid below 1000 when the Max is too low for
// 21 distinct region-ladder values. Coarsest first — the grid prefers the
// biggest round step that still fits 21 distinct values ending at the Max
// (Max 10000 → 100s, Max 2000 → 25s, Max 500 → 10s, …).
const NICE_FILL_STEPS = [100, 50, 25, 20, 10, 5, 2, 1];

// The grid never shows nits below this. A full 21-cell grid can't start at
// exactly 300 for every Max (the span 300→1000 only divides evenly into a
// nice step for Max 4000 and 10000), so the fill uses the closest round
// start ≥ this floor — the minimum is always ≥ 300 (e.g. Max 900 → 400,
// Max 1000 → 500, Max 10000 → 300), never 0. This is only the *natural*
// floor per Max; monotonicFloorForStop below then raises it so the minimum
// never decreases as the Max slider moves up.
const HIGH_NITS_GRID_MIN_FLOOR = 300;

// Largest nice round step that keeps `count` values ending at `end` with
// every value at/above `floor`. Used to build the natural (round) grid.
function naturalFillStep(end, count, floor) {
  return NICE_FILL_STEPS.find((st) => end - st * (count - 1) >= floor) ?? NICE_FILL_STEPS[NICE_FILL_STEPS.length - 1];
}

// The natural (round, floor-300) grid minimum for a Max — the start value the
// current design would use before monotonicity is enforced. `cells` lets a
// fullscreen grid grow past the fixed 21 (e.g. 28 cells on 16:9) while still
// honoring the 300-nits floor.
function naturalGridMin(max, cells = HIGH_NITS_CELLS) {
  const top = regionValues(max);
  if (max < 1000) {
    return max - naturalFillStep(max, cells, HIGH_NITS_GRID_MIN_FLOOR) * (cells - 1);
  }
  const belowCount = cells - top.length;
  return 1000 - naturalFillStep(1000, belowCount, HIGH_NITS_GRID_MIN_FLOOR) * (belowCount - 1);
}

// The region values above 1000 for a Max (500-step 1500…5000, then 1000-step
// 6000… up to Max). Shared by the grid builder and the natural-min helper so
// they can never disagree about how many cells the region consumes.
function regionValues(max) {
  const top = [];
  for (let v = 1500; v <= Math.min(5000, max); v += 500) top.push(v);
  for (let v = 6000; v <= max; v += 1000) top.push(v);
  return top;
}

// The grid minimum for a Max when the fill starts at `floor`. The step is
// the largest whole step that keeps all cells ascending, ending at Max and
// staying at/above the floor (a fine/non-round step when the natural round
// one would dip below the floor). Shared by the floor precompute and the
// grid builder so the precomputed minimum always matches the built grid.
function gridMinForFloor(max, floor, cells = HIGH_NITS_CELLS) {
  const top = regionValues(max);
  if (max < 1000) {
    const s = Math.floor((max - floor) / (cells - 1));
    return floor; // step <= span/20 keeps the start exactly at the floor
  }
  const belowCount = cells - top.length;
  const s = Math.floor((1000 - floor) / (belowCount - 1));
  return 1000 - (belowCount - 1) * s;
}

// Monotonic minimum floor per slider stop. As the Max slider moves up, the
// grid minimum must NEVER decrease (a higher Max showing a lower min than a
// lower Max is exactly the inconsistency reported). Stops are walked in
// ascending order; each stop's floor is the highest minimum achieved so far
// (its own natural minimum, or the previous stop's carried *grid* minimum —
// the grid can start above its floor when the fill step rounds, so carrying
// the natural floor alone could still let a later grid dip). When the
// natural round grid would dip below the carried floor (Max 4000/10000
// consume so many region cells that the round fill bottoms out at 300), the
// fill switches to a fine non-round step anchored at the carried floor — the
// user accepted non-round low values as the price of a monotonic minimum.
const HIGH_NITS_STOP_FLOORS = new Map();
(() => {
  let carried = 0;
  for (const stop of HIGH_NITS_MAX_STOPS) {
    const floor = Math.max(carried, naturalGridMin(stop));
    const gridMin = gridMinForFloor(stop, floor);
    HIGH_NITS_STOP_FLOORS.set(stop, gridMin);
    carried = gridMin;
  }
})();

// The monotonic floor for any Max. Slider stops look it up directly; an
// off-stop value (e.g. an old persisted Max before snapping) uses the first
// stop at or above it, falling back to its natural minimum. Off-stop grids
// are a transient fallback only — the frontend always snaps the Max to a
// slider stop before rendering, so in practice the grid is only built from
// an exact stop and always ends exactly at the Max.
function monotonicFloorForMax(max) {
  const exact = HIGH_NITS_STOP_FLOORS.get(max);
  if (exact !== undefined) return exact;
  for (const stop of HIGH_NITS_MAX_STOPS) {
    if (stop >= max) {
      const floor = HIGH_NITS_STOP_FLOORS.get(stop);
      if (floor !== undefined) return floor;
      break;
    }
  }
  return naturalGridMin(max);
}

// The grid values for a given Max and cell count, ending EXACTLY at Max.
// Above 1000 the region steps are used (500-step 1500…5000, 1000-step above
// 5000). Below 1000, the fill starts at the monotonic floor
// (monotonicFloorForMax) and uses the largest step that keeps all cells
// ascending and ending at Max; when the natural round step would put the
// minimum below the carried floor, the step becomes fine/non-round (e.g.
// Max 4000 → 636, 662, 688…; Max 10000 → 664, 712, 760…).
//
// `cells` defaults to the fixed 21 (3×7) normal-mode grid. Fullscreen passes
// a larger count (the aspect-fit grid, e.g. 28 on 16:9) so every slot shows
// a real value — the extra cells simply make the below-1000 fill finer.
export function highNitsGridCodes(max, cells = HIGH_NITS_CELLS) {
  // The monotonic floor map is only valid for the fixed 21-cell grid (the
  // slider's source of truth). Larger fullscreen grids use the natural
  // 300-nits floor for their own cell count instead.
  const floor = cells === HIGH_NITS_CELLS ? monotonicFloorForMax(max) : naturalGridMin(max, cells);
  const top = regionValues(max);
  if (max < 1000) {
    // Entire grid below 1000: step = biggest step that keeps `cells` values
    // ending at max at/above the floor (e.g. Max 400 → 300..400 @ 5).
    const s = Math.max(1, Math.floor((max - floor) / (cells - 1)));
    return Array.from({ length: cells }, (_, i) => floor + i * s);
  }
  // Bridge from the floor up to 1000 (fine step when needed), then the
  // region steps above.
  const belowCount = Math.max(2, cells - top.length);
  const s = Math.max(1, Math.floor((1000 - floor) / (belowCount - 1)));
  const start = 1000 - (belowCount - 1) * s;
  const below = Array.from({ length: belowCount }, (_, i) => start + i * s);
  return below.concat(top);
}

// The step readout for a Max, following the region rule the user described:
// 1000-step above 5000, 500-step from 1000 to 5000, 100-step below 1000.
export function highNitsStepForMax(max) {
  if (max > 5000) return 1000;
  if (max > 1000) return 500;
  return 100;
}

// Evenly samples `count` integer values across [start, end] (inclusive).
// In SDR the 24-value bright RGB range (232..255) is spread over the fixed
// 21-cell grid instead of being sliced (a plain slice would drop 255).
export function sampleRange(start, end, count) {
  const out = [];
  for (let i = 0; i < count; i++) out.push(Math.round(start + (i * (end - start)) / (count - 1)));
  return out;
}

// SDR label/render codes for a grid of `cells` (default: the fixed 21-cell
// grid). Single source of truth so the DOM labels and the shader values can
// never drift apart. Larger fullscreen grids sample the bright RGB range
// more finely.
export function highNitsSdrCodes(cells = HIGH_NITS_CELLS) {
  return sampleRange(HIGH_SDR_RANGE[0], HIGH_SDR_RANGE[HIGH_SDR_RANGE.length - 1], cells);
}

// SDR bright range: 255 down to 232, i.e. 24 values ascending (232..255),
// same quantity as the SDR black codes. No midrange values.
export const HIGH_SDR_RANGE = [];
for (let c = 232; c <= 255; c++) HIGH_SDR_RANGE.push(c);

// Strip registry — adding a third strip later is one new entry here.
// `label` is the heading button / context-menu text; `title` its tooltip.
export const STRIPS = [
  {
    id: 'black',
    label: 'Black',
    title: 'Black calibration strip — dark PQ/RGB ramp for black level tuning',
  },
  {
    id: 'highNits',
    label: 'Brightness',
    title: 'Brightness grid strip — ascending nits cells to judge peak output',
  },
];

// nits → scRGB (linear) relative to 80 nits = 1.0 reference white, matching
// the black strip's PQ mapping convention (pqCodeToScrgb divides by 80).
export function nitsToScrgb(nits) {
  return nits / 80;
}

// Render values for the High Nits strip. HDR: the region-stepped grid codes
// (see highNitsGridCodes) → scRGB → sRGB-encoded. SDR: the bright RGB range
// sampled over the same number of cells, through the sRGB round-trip like
// the black SDR strip. `cells` defaults to the fixed 21; fullscreen passes
// the aspect-fit count so no slot is left empty.
export function computeHighNitsValues(max, isHdr, cells = HIGH_NITS_CELLS) {
  const codes = isHdr ? highNitsGridCodes(max, cells) : highNitsSdrCodes(cells);
  return codes.map((code) => (
    isHdr
      ? linearToSrgb(nitsToScrgb(code))
      : linearToSrgb(srgbToLinear(code / 255))
  ));
}

export function srgbToLinear(c) {
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

export function linearToSrgb(c) {
  return c <= 0.0031308 ? c * 12.92 : 1.055 * Math.pow(c, 1 / 2.4) - 0.055;
}

// ITU-R BT.2400 reference/diffuse white for HDR (nits). At this SDR white
// level, Chromium tone-maps HDR screenshots with their intended brightness,
// and the SDR/HDR comparison rows in the Experimental tab align exactly.
// Shared by TabExperimental (reset default) and SdrHdrCompare (HDR row).
export const HDR_REFERENCE_NITS = 203;

// The cell-grid shader used by the calibration strip (TabCrush.svelte). Also
// reused verbatim by the Experimental tab's SDR-vs-HDR comparison canvas so
// the two renderers can never drift apart.
export const CELL_GRID_SHADER = `struct Cb{values:array<vec4<f32>,12>,counts:vec4<f32>}@group(0)@binding(0)var<uniform>cb:Cb;
struct VO{@builtin(position)p:vec4f,@location(0)uv:vec2f}
@vertex fn vs(@builtin(vertex_index)i:u32)->VO{
  let pos=array(vec2f(-1,1),vec2f(3,1),vec2f(-1,-3));
  let p=pos[i];
  return VO(vec4f(p,0,1),p*vec2f(.5,-.5)+vec2f(.5,.5));
}
@fragment fn fs(vin:VO)->@location(0)vec4f{
  let cells=i32(cb.counts.x);let cols=i32(cb.counts.y);let rows=i32(cb.counts.z);let gf=cb.counts.w;
  let sw=(1-gf*f32(cols-1))/f32(cols);let cw=sw+gf;
  let sh=(1-gf*f32(rows-1))/f32(rows);let ch=sh+gf;
  let col=i32(floor(vin.uv.x/cw));let row=i32(floor(vin.uv.y/ch));
  let xf=fract(vin.uv.x/cw)*cw;let yf=fract(vin.uv.y/ch)*ch;
  if(xf>sw||yf>sh||col>=cols||row>=rows){return vec4f(0);}
  let idx=row*cols+col;
  if(idx>=cells){return vec4f(0);}
  let g=idx/4;return vec4f(cb.values[g][idx%4]);
}`;

export function pqCodeToScrgb(code) {
  if (code <= 64) return 0;
  const v = Math.max(0, Math.min(1, (code - 64) / 876));
  const vm = Math.pow(v, 1 / 78.84375);
  const num = Math.max(0, vm - 0.8359375);
  return 10000 * Math.pow(num / (18.8515625 - 18.6875 * vm), 1 / 0.1593017578125) / 80;
}

export function computeHdrValues() {
  return ALL_PQ_CODES.map(c => linearToSrgb(pqCodeToScrgb(c)));
}

export function computeSdrValues() {
  return ALL_SDR_CODES.map(c => linearToSrgb(srgbToLinear(c / 255)));
}
