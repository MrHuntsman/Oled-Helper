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
