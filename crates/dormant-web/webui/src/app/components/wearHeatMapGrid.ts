/** Wear-heat-map data model + colour ramps — grid normalization and the
 * deviation/heat colour helpers consumed by the WearHeatMap component and
 * the DisplayDetail legend. Pure data, no React. */

/** Absolute-hours floor below which deviation colouring is suppressed.
 * A panel with mean < 1 h has insufficient accumulated wear for the
 * (hours − mean) / mean ratio to carry meaning — a 20 % relative
 * spread on 0.1 h is two hundredths of an hour of absolute wear. */
export const WEAR_FLOOR_HOURS = 1.0;
import type { WearDetail } from "../../api/types";

export const HEAT_RAMP_STOPS: readonly [number, number, number, number][] = [
  [0.0, 60, 70, 90],
  [0.32, 195, 232, 141],
  [0.62, 255, 199, 119],
  [0.82, 255, 150, 108],
  [1.0, 255, 117, 127],
];

/** Returns the legacy normalized-heat ramp used by the absolute-hours legend. */
export function heatColor(value: number): string {
  const v = Math.max(0, Math.min(1, value));
  let lo = HEAT_RAMP_STOPS[0];
  let hi = HEAT_RAMP_STOPS[HEAT_RAMP_STOPS.length - 1];
  for (let i = 0; i < HEAT_RAMP_STOPS.length - 1; i++) {
    if (v >= HEAT_RAMP_STOPS[i][0] && v <= HEAT_RAMP_STOPS[i + 1][0]) {
      lo = HEAT_RAMP_STOPS[i];
      hi = HEAT_RAMP_STOPS[i + 1];
      break;
    }
  }
  const span = hi[0] - lo[0];
  const t = span === 0 ? 0 : (v - lo[0]) / span;
  const r = Math.round(lo[1] + (hi[1] - lo[1]) * t);
  const g = Math.round(lo[2] + (hi[2] - lo[2]) * t);
  const b = Math.round(lo[3] + (hi[3] - lo[3]) * t);
  const alpha = Math.round((0.22 + v * 0.78) * 100) / 100;
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

const NEUTRAL_COLOR = "rgba(195, 232, 141, 0.47)";
const COOL_COLOR = [86, 156, 214] as const;
const AMBER_COLOR = [255, 199, 119] as const;
const RED_COLOR = [255, 117, 127] as const;

function mixColor(from: readonly number[], to: readonly number[], amount: number): string {
  const t = Math.max(0, Math.min(1, amount));
  const channels = from.map((value, index) => Math.round(value + (to[index] - value) * t));
  return `rgb(${channels[0]}, ${channels[1]}, ${channels[2]})`;
}

export function deviationColor(deviation: number): string {
  if (deviation >= -0.05 && deviation <= 0.05) return NEUTRAL_COLOR;
  if (deviation > 0.05) {
    return mixColor(AMBER_COLOR, RED_COLOR, (deviation - 0.05) / 0.15);
  }
  return mixColor(COOL_COLOR, [195, 232, 141], (deviation + 0.2) / 0.15);
}

export interface NormalizedWearGrid {
  rows: number;
  cols: number;
  weightedHours: number[];
  heat: number[];
  deviations: number[];
  meanHours: number;
  sampleCount: number;
  hasSpatialVariation: boolean;
  hasGridSamples: boolean;
  hasHeatSamples: boolean;
  averageHeat: number | null;
  uniformity: number | null;
}

export function normalizeWearGrid(detail: WearDetail | undefined): NormalizedWearGrid {
  const rows = detail && Number.isFinite(detail.grid_rows) ? Math.max(0, Math.trunc(detail.grid_rows)) : 0;
  const cols = detail && Number.isFinite(detail.grid_cols) ? Math.max(0, Math.trunc(detail.grid_cols)) : 0;
  const size = rows * cols;
  const weightedHours = Array.from({ length: size }, (_, index) => {
    const value = detail?.cells[index];
    return typeof value === "number" && Number.isFinite(value) ? Math.max(0, value) : 0;
  });
  const heat = Array.from({ length: size }, (_, index) => {
    const value = detail?.heat[index];
    return typeof value === "number" && Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : 0;
  });
  const validGridSamples = (detail?.cells ?? []).slice(0, size).filter(Number.isFinite).length;
  const validHeatSamples = (detail?.heat ?? []).slice(0, size).filter(Number.isFinite).length;
  const hasGridSamples = size > 0 && validGridSamples > 0;
  const hasHeatSamples = size > 0 && validHeatSamples > 0;
  const meanHours = hasGridSamples ? weightedHours.reduce((sum, value) => sum + value, 0) / size : 0;
  const deviations = weightedHours.map((hours) => meanHours > 0 ? (hours - meanHours) / meanHours : 0);
  const averageHeat = hasHeatSamples ? heat.reduce((sum, value) => sum + value, 0) / size : null;
  const uniformity = hasHeatSamples ? Math.max(0, 1 - (Math.max(...heat) - Math.min(...heat))) : null;
  return {
    rows, cols, weightedHours, heat, deviations, meanHours,
    sampleCount: detail?.sample_count ?? 0,
    hasSpatialVariation: deviations.some((deviation) => Math.abs(deviation) > 0.05),
    hasGridSamples, hasHeatSamples, averageHeat, uniformity,
  };
}
