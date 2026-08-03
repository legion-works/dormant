/** Wear heat map colour and interaction regression tests. */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { heatColor, normalizeWearGrid, WearHeatMap } from "../app/components/WearHeatMap";
import type { WearDetail } from "../api/types";

afterEach(() => cleanup());

describe("heatColor", () => {
  it("returns the cold-stop color at v=0", () => {
    expect(heatColor(0)).toBe("rgba(60, 70, 90, 0.22)");
  });

  it("returns the hot-stop color at v=1", () => {
    expect(heatColor(1)).toBe("rgba(255, 117, 127, 1)");
  });

  it("interpolates linearly at an exact stop", () => {
    expect(heatColor(0.32)).toBe("rgba(195, 232, 141, 0.47)");
  });

  it("interpolates a midpoint between stops", () => {
    expect(heatColor(0.47)).toBe("rgba(225, 216, 130, 0.59)");
  });

  it("clamps values above 1 and below 0 to the ramp endpoints", () => {
    expect(heatColor(1.5)).toBe(heatColor(1));
    expect(heatColor(-0.5)).toBe(heatColor(0));
  });
});

describe("WearHeatMap deviation rendering", () => {
  function detail(cells: number[], heat: number[], sampleCount = 48): WearDetail {
    return {
      display: "panel-main", display_name: "main", panel_type: "unknown",
      total_on_hours: 100, max_cell_hours: Math.max(...cells), sample_count: sampleCount,
      advisory: false, hours_since_long_dwell: 0, grid_rows: 2, grid_cols: 2, cells, heat,
    };
  }

  it.each([
    ["uniform", [100, 100, 100, 100], [0, 0.33, 0.67, 1]],
    ["within ±1%", [99, 101, 100, 100], [0, 1, 0.5, 0.5]],
  ])("renders %s wear as one neutral colour with honest sample copy", (_name, cells, heat) => {
    render(<WearHeatMap display="main" grid={normalizeWearGrid(detail(cells, heat))} />);
    const colors = new Set(screen.getAllByRole("gridcell").map((cell) => cell.style.backgroundColor));
    expect(colors).toHaveLength(1);
    expect(screen.getByText("no spatial variation yet — 48 samples")).toBeInTheDocument();
  });

  it("maps only a cell 20% above the mean to full red", () => {
    const grid = normalizeWearGrid(detail([120, 93.333333, 93.333333, 93.333335], [0.95, 0.2, 0.3, 0.4]));
    render(<WearHeatMap display="main" grid={grid} />);
    const cells = screen.getAllByRole("gridcell");
    expect(cells[0]).toHaveStyle({ backgroundColor: "rgb(255, 117, 127)" });
    expect(cells.slice(1).map((cell) => cell.style.backgroundColor)).not.toContain("rgb(255, 117, 127)");
  });

  it("exposes one detailed accessible tooltip when a cell receives focus", () => {
    const grid = normalizeWearGrid(detail([120, 93.333333, 93.333333, 93.333335], [0.95, 0.2, 0.3, 0.4]));
    render(<WearHeatMap display="main" grid={grid} />);
    const cell = screen.getAllByRole("gridcell")[0];
    cell.focus();
    fireEvent.focus(cell);
    expect(cell).toHaveFocus();
    const tooltip = screen.getByRole("tooltip");
    expect(cell).toHaveAttribute("aria-describedby", tooltip.id);
    expect(tooltip).toHaveTextContent("row 1, column 1");
    expect(tooltip).toHaveTextContent(/top.*0–50%.*0–50%/i);
    expect(tooltip).toHaveTextContent("120.00 hours");
    expect(tooltip).toHaveTextContent("+20% above mean");
    expect(tooltip).toHaveTextContent("95% normalized heat");
  });
});
