/**
 * Panel wear heat map — renders a display's per-cell brightness-weighted
 * on-hours / normalized heat grid as an accessible `role="grid"`.
 *
 * The backend returns row-major raw `cells` (brightness-weighted on-hours)
 * and normalized `heat` (`0..=1`), but JSON/manual fixtures can still be
 * short, long, empty, or non-finite. `normalizeWearGrid` is the single
 * source of truth: it pads/truncates both vectors to `grid_rows *
 * grid_cols`, maps every non-finite value to zero, and derives
 * `averageHeat`/`uniformity` from that same clamped vector — callers must
 * not recompute those independently.
 */
import type { WearDetail } from "../../api/types";
import "./WearHeatMap.css";

/**
 * Heat-map ramp stops, per the design handoff ("Design tokens" §Heat-map
 * ramp): `[v, r, g, b]`. No DS token exists for these — the handoff gives
 * literal ramp values with no token, so the raw RGB triples live here as
 * the single source (both `heatColor` and the legend gradient CSS derive
 * from the same five stops).
 */
const HEAT_RAMP_STOPS: readonly [number, number, number, number][] = [
  [0.0, 60, 70, 90],
  [0.32, 195, 232, 141], // #C3E88D
  [0.62, 255, 199, 119], // #FFC777
  [0.82, 255, 150, 108], // #FF966C
  [1.0, 255, 117, 127], // #FF757F
];

/**
 * Continuous heat ramp: clamps `v` to 0..1, linearly interpolates the RGB
 * between the bracketing stops, and derives alpha as `0.22 + v*0.78` — at
 * `v=0` every cell still renders at 0.22 alpha (visible on the sunken
 * tile), never a black void, so the uniform/all-zero case reads as a
 * legible cold map instead of nothing.
 */
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

export interface NormalizedWearGrid {
  rows: number;
  cols: number;
  weightedHours: number[];
  heat: number[];
  hasGridSamples: boolean;
  hasHeatSamples: boolean;
  averageHeat: number | null;
  uniformity: number | null;
}

export function normalizeWearGrid(detail: WearDetail | undefined): NormalizedWearGrid {
  const rows = detail && Number.isFinite(detail.grid_rows)
    ? Math.max(0, Math.trunc(detail.grid_rows))
    : 0;
  const cols = detail && Number.isFinite(detail.grid_cols)
    ? Math.max(0, Math.trunc(detail.grid_cols))
    : 0;
  const size = rows * cols;
  const weightedHours = Array.from({ length: size }, (_, index) => {
    const value = detail?.cells[index];
    return typeof value === "number" && Number.isFinite(value) ? Math.max(0, value) : 0;
  });
  const heat = Array.from({ length: size }, (_, index) => {
    const value = detail?.heat[index];
    return typeof value === "number" && Number.isFinite(value)
      ? Math.max(0, Math.min(1, value))
      : 0;
  });
  const validGridSamples = (detail?.cells ?? []).slice(0, size).filter(Number.isFinite).length;
  const validHeatSamples = (detail?.heat ?? []).slice(0, size).filter(Number.isFinite).length;
  const hasGridSamples = size > 0 && validGridSamples > 0;
  const hasHeatSamples = size > 0 && validHeatSamples > 0;
  const averageHeat = hasHeatSamples
    ? heat.reduce((sum, value) => sum + value, 0) / size
    : null;
  const uniformity = hasHeatSamples
    ? Math.max(0, 1 - (Math.max(...heat) - Math.min(...heat)))
    : null;
  return {
    rows,
    cols,
    weightedHours,
    heat,
    hasGridSamples,
    hasHeatSamples,
    averageHeat,
    uniformity,
  };
}

export function WearHeatMap({ display, grid }: { display: string; grid: NormalizedWearGrid }) {
  if (grid.rows === 0 || grid.cols === 0 || (!grid.hasGridSamples && !grid.hasHeatSamples)) {
    return (
      <div className="wear-heat-map__empty">
        No spatial wear samples for this display yet.
      </div>
    );
  }

  return (
    <div
      className="wear-heat-map"
      role="grid"
      aria-label={`${display} panel wear heat map`}
      style={{
        gridTemplateColumns: `repeat(${grid.cols}, minmax(0, 1fr))`,
        gridTemplateRows: `repeat(${grid.rows}, 1fr)`,
        aspectRatio: `${grid.cols} / ${grid.rows}`,
      }}
    >
      {grid.heat.map((intensity, index) => {
        const row = Math.floor(index / grid.cols) + 1;
        const col = (index % grid.cols) + 1;
        const hours = grid.weightedHours[index];
        const percent = Math.round(intensity * 100);
        const label = `row ${row}, column ${col}: ${hours.toFixed(2)} brightness-weighted hours at ${percent} percent normalized heat`;
        return (
          <div
            key={index}
            role="gridcell"
            aria-label={label}
            title={`${hours.toFixed(1)} on-hours`}
            className="wear-heat-map__cell"
            style={{ backgroundColor: heatColor(intensity) }}
          />
        );
      })}
    </div>
  );
}
