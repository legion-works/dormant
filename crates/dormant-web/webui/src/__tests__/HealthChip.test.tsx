import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import HealthChip from "../app/components/HealthChip";
import type { ControllerHealth } from "../api/types";

afterEach(cleanup);

function healthyCtl(name: string, role: "primary" | "fallback" = "primary"): ControllerHealth {
  return { name, role, healthy: true };
}

function unhealthyCtl(
  name: string,
  detail: string,
  role: "primary" | "fallback" = "primary",
): ControllerHealth {
  return { name, role, healthy: false, detail };
}

describe("HealthChip", () => {
  it("renders a healthy primary controller", () => {
    render(<HealthChip health={healthyCtl("ddcci")} />);
    expect(screen.getByText("ddcci")).toBeInTheDocument();
    expect(screen.getByText("primary")).toBeInTheDocument();
    // Healthy chip must not carry the unhealthy modifier.
    const chip = screen.getByText("ddcci").closest(".health-chip")!;
    expect(chip.className).not.toContain("health-chip--unhealthy");
  });

  it("renders an unhealthy fallback controller with detail", () => {
    render(
      <HealthChip
        health={unhealthyCtl("samsung-tizen", "E_DISPLAY_IO: connection refused", "fallback")}
      />,
    );
    expect(screen.getByText("samsung-tizen")).toBeInTheDocument();
    expect(screen.getByText("fallback")).toBeInTheDocument();
    const chip = screen.getByText("samsung-tizen").closest(".health-chip")!;
    expect(chip.className).toContain("health-chip--unhealthy");
    // Tooltip title
    expect(chip.getAttribute("title")).toBe("E_DISPLAY_IO: connection refused");
    // Inline detail
    expect(
      screen.getByText(/E_DISPLAY_IO: connection refused/),
    ).toBeInTheDocument();
  });

  it("renders an unhealthy controller without detail (no suffix)", () => {
    render(
      <HealthChip
        health={{ name: "command", role: "primary", healthy: false }}
      />,
    );
    expect(screen.getByText("command")).toBeInTheDocument();
    // No detail suffix element
    const chip = screen.getByText("command").closest(".health-chip")!;
    const detailEl = chip.querySelector(".health-chip__detail");
    expect(detailEl).toBeNull();
  });

  it("truncates long detail strings", () => {
    const long = "X".repeat(200);
    render(
      <HealthChip
        health={unhealthyCtl("ddcci", long)}
      />,
    );
    // The title keeps the full string.
    const chip = screen.getByText("ddcci").closest(".health-chip")!;
    expect(chip.getAttribute("title")).toBe(long);
    // The visible suffix is truncated.
    const detailEl = chip.querySelector(".health-chip__detail")!;
    expect(detailEl.textContent!.length).toBeLessThan(long.length);
    expect(detailEl.textContent).toContain("…");
  });
});
