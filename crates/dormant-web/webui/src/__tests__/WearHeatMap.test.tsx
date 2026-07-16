/**
 * WearHeatMap ramp tests — pins the continuous heat-color interpolation
 * against the design handoff's exact ramp spec (README.md "Design tokens"
 * §Heat-map ramp): stops at 0/0.32/0.62/0.82/1.0, linear interpolation
 * between them, alpha = 0.22 + v*0.78.
 */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { heatColor, normalizeWearGrid, WearHeatMap } from "../app/components/WearHeatMap";
import type { WearDetail } from "../api/types";

afterEach(() => cleanup());

describe("heatColor", () => {
  it("returns the cold-stop color at v=0 (alpha 0.22 — visible on the sunken tile, never a black void)", () => {
    expect(heatColor(0)).toBe("rgba(60, 70, 90, 0.22)");
  });

  it("returns the hot-stop color at v=1 (full opacity)", () => {
    expect(heatColor(1)).toBe("rgba(255, 117, 127, 1)");
  });

  it("interpolates linearly at an exact stop (v=0.32 = the green stop, alpha 0.22 + 0.32*0.78 = 0.4696)", () => {
    // C3E88D = rgb(195, 232, 141); alpha = 0.22 + 0.32*0.78 = 0.4696 -> rounds to 0.47
    expect(heatColor(0.32)).toBe("rgba(195, 232, 141, 0.47)");
  });

  it("interpolates a midpoint between stops (v=0.47, halfway between 0.32 and 0.62)", () => {
    // Midpoint of [195,232,141]->[255,199,119] at t=0.5: r=225, g=215.5->216, b=130
    // alpha = 0.22 + 0.47*0.78 = 0.5866 -> rounds to 0.59
    expect(heatColor(0.47)).toBe("rgba(225, 216, 130, 0.59)");
  });

  it("clamps values above 1 and below 0 to the ramp's endpoints", () => {
    expect(heatColor(1.5)).toBe(heatColor(1));
    expect(heatColor(-0.5)).toBe(heatColor(0));
  });
});

describe("WearHeatMap uniform-data case", () => {
  it("renders every cell at the cold-stop 0.22-alpha color when all heat values are 0 (visible, not a black void)", () => {
    const detail: WearDetail = {
      display: "panel-uniform",
      display_name: "uniform",
      panel_type: "unknown",
      total_on_hours: 10,
      sample_count: 4,
      advisory: false,
      hours_since_long_dwell: 0,
      grid_rows: 2,
      grid_cols: 2,
      cells: [0, 0, 0, 0],
      heat: [0, 0, 0, 0],
    };
    const grid = normalizeWearGrid(detail);
    render(<WearHeatMap display="uniform" grid={grid} />);

    const cells = screen.getAllByRole("gridcell");
    expect(cells).toHaveLength(4);
    for (const cell of cells) {
      expect(cell).toHaveStyle({ backgroundColor: "rgba(60, 70, 90, 0.22)" });
    }
  });
});
