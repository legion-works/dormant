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

export const WEAR_HEAT_COLORS = [
  "var(--wear-heat-0)",
  "var(--wear-heat-1)",
  "var(--wear-heat-2)",
  "var(--wear-heat-3)",
  "var(--wear-heat-4)",
] as const;

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
      style={{ gridTemplateColumns: `repeat(${grid.cols}, minmax(0, 1fr))` }}
    >
      {grid.heat.map((intensity, index) => {
        const row = Math.floor(index / grid.cols) + 1;
        const col = (index % grid.cols) + 1;
        const hours = grid.weightedHours[index];
        const percent = Math.round(intensity * 100);
        const label = `row ${row}, column ${col}: ${hours.toFixed(2)} brightness-weighted hours at ${percent} percent normalized heat`;
        const colorIndex = Math.min(4, Math.floor(intensity * 5));
        return (
          <div
            key={index}
            role="gridcell"
            aria-label={label}
            title={label}
            className="wear-heat-map__cell"
            style={{ backgroundColor: WEAR_HEAT_COLORS[colorIndex] }}
          />
        );
      })}
    </div>
  );
}
