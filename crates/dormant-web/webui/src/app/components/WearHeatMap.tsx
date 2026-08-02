/** Panel wear heat map derived from each cell's deviation from the panel mean. */
import { useState } from "react";
import type { WearDetail } from "../../api/types";
import "./WearHeatMap.css";

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

function regionLabel(row: number, col: number, rows: number, cols: number): string {
  const vertical = row <= rows / 2 ? "top" : "bottom";
  const horizontal = col <= cols / 2 ? "left" : "right";
  const xStart = Math.round(((col - 1) / cols) * 100);
  const xEnd = Math.round((col / cols) * 100);
  const yStart = Math.round(((row - 1) / rows) * 100);
  const yEnd = Math.round((row / rows) * 100);
  return `${vertical}-${horizontal} region, ${xStart}–${xEnd}% across, ${yStart}–${yEnd}% down`;
}

function deltaLabel(deviation: number): string {
  const percent = Math.round(Math.abs(deviation) * 100);
  if (percent === 0) return "at mean";
  return deviation > 0 ? `+${percent}% above mean` : `−${percent}% below mean`;
}

export function WearHeatMap({ display, grid }: { display: string; grid: NormalizedWearGrid }) {
  const [activeCell, setActiveCell] = useState<number | null>(null);
  if (grid.rows === 0 || grid.cols === 0 || (!grid.hasGridSamples && !grid.hasHeatSamples)) {
    return <div className="wear-heat-map__empty">No spatial wear samples for this display yet.</div>;
  }

  return (
    <>
      <div className="wear-heat-map" role="grid" aria-label={`${display} panel wear heat map`} style={{
        gridTemplateColumns: `repeat(${grid.cols}, minmax(0, 1fr))`,
        gridTemplateRows: `repeat(${grid.rows}, 1fr)`,
        aspectRatio: `${grid.cols} / ${grid.rows}`,
      }}>
        {grid.heat.map((_, index) => {
          const row = Math.floor(index / grid.cols) + 1;
          const col = (index % grid.cols) + 1;
          const deviation = grid.deviations[index];
          const tooltipId = `wear-cell-tooltip-${index}`;
          return (
            <div key={index} role="gridcell" tabIndex={0} aria-describedby={activeCell === index ? tooltipId : undefined}
              aria-label={`row ${row}, column ${col}`} className="wear-heat-map__cell"
              style={{ backgroundColor: deviationColor(deviation) }}
              onFocus={() => setActiveCell(index)} onBlur={() => setActiveCell(null)}
              onMouseEnter={() => setActiveCell(index)} onMouseLeave={() => setActiveCell(null)} />
          );
        })}
      </div>
      {!grid.hasSpatialVariation && (
        <div className="wear-heat-map__variation-note">no spatial variation yet — {grid.sampleCount.toLocaleString()} samples</div>
      )}
      {activeCell !== null && (() => {
        const row = Math.floor(activeCell / grid.cols) + 1;
        const col = (activeCell % grid.cols) + 1;
        return <div id={`wear-cell-tooltip-${activeCell}`} role="tooltip" className="wear-heat-map__tooltip">
          row {row}, column {col} · {regionLabel(row, col, grid.rows, grid.cols)} · {grid.weightedHours[activeCell].toFixed(2)} hours · {deltaLabel(grid.deviations[activeCell])} · {Math.round(grid.heat[activeCell] * 100)}% normalized heat
        </div>;
      })()}
    </>
  );
}
