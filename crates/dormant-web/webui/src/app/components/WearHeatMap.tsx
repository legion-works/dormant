/** Panel wear heat map derived from each cell's deviation from the panel mean. */
import { useState } from "react";
import { deviationColor, type NormalizedWearGrid } from "./wearHeatMapGrid";
import "./WearHeatMap.css";

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
