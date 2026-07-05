/**
 * Unit tests for StatusChip — the state → DS-color mapper.
 *
 * Tests that each state kind maps to the correct status class
 * so Batch B views (Events, Config, Doctor) can rely on the
 * same mapping.
 */
import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import StatusChip from "../app/components/StatusChip";

describe("StatusChip", () => {
  // Green states
  it.each(["present", "active", "waking", "ok"])(
    "%s maps to success class",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el).not.toBeNull();
      expect(el!.className).toContain("status-chip--success");
      expect(el!.querySelector(".status-chip__dot")).not.toBeNull();
    },
  );

  // Blue states
  it.each(["absent", "blanked"])(
    "%s maps to blue class",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).toContain("status-chip--blue");
    },
  );

  // Yellow states
  it.each(["grace", "blanking", "unavailable"])(
    "%s maps to warning class",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).toContain("status-chip--warning");
    },
  );

  // Special states
  it("paused maps to amber class", () => {
    const { container } = render(<StatusChip kind="paused" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--amber");
  });

  it("inhibited maps to purple class", () => {
    const { container } = render(<StatusChip kind="inhibited" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--purple");
  });

  it.each(["fail", "wake_retry"])(
    "%s maps to danger class",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).toContain("status-chip--danger");
    },
  );

  it("renders custom label when provided", () => {
    const { container } = render(<StatusChip kind="active" label="ON" />);
    const el = container.querySelector(".status-chip");
    expect(el?.textContent).toContain("ON");
  });

  it("hides dot when dot=false", () => {
    const { container } = render(<StatusChip kind="active" dot={false} />);
    const el = container.querySelector(".status-chip__dot");
    expect(el).toBeNull();
  });

  it("renders unknown state as muted fallback", () => {
    const { container } = render(<StatusChip kind="some-weird-state" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--muted");
    expect(el?.textContent).toContain("some-weird-state");
  });

  it("appends className prop", () => {
    const { container } = render(<StatusChip kind="active" className="extra" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("extra");
  });
});
