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

// Fixed nits ladder, ascending. Coarse at the top, fine at the bottom
// (user: "go from 10000 to 4000 to 2000 to 1000, and then start slowing
// down to get more granularity"). This is a placeholder the user will
// refine — edit this single array.
export const HIGH_NITS_LADDER = [
  100, 150, 200, 250, 300, 350, 400, 500, 600, 700, 800,
  1000, 1200, 1500, 2000, 3000, 4000, 5000, 6000, 8000, 10000,
];

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
    label: 'High Nits',
    title: 'High nits grid strip — ascending brightness cells to judge peak output',
  },
];

// nits → scRGB (linear) relative to 80 nits = 1.0 reference white, matching
// the black strip's PQ mapping convention (pqCodeToScrgb divides by 80).
export function nitsToScrgb(nits) {
  return nits / 80;
}

// Window into a ladder for a given square count. More cells = finer
// (lower-nits) steps; fewer cells = coarser spread reaching the top.
// Returns a contiguous slice; ordering stays ascending.
export function highNitsWindow(ladder, squareCount) {
  const M = ladder.length;
  const n = Math.max(2, Math.min(squareCount, MAX_SQUARES));
  if (M <= n) return ladder.slice(0, n);
  const f = (n - 2) / (MAX_SQUARES - 2);
  const start = Math.round((M - n) * (1 - f));
  return ladder.slice(start, start + n);
}

// Render values for the High Nits strip. HDR: nits → scRGB → sRGB-encoded.
// SDR: code/255 through the sRGB round-trip like the black SDR strip.
export function computeHighNitsValues(squareCount, isHdr) {
  const codes = highNitsWindow(isHdr ? HIGH_NITS_LADDER : HIGH_SDR_RANGE, squareCount);
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
