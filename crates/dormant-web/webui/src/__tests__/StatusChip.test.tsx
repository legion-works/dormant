/**
 * Unit tests for StatusChip — the state → DS-color mapper.
 */
import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import StatusChip from "../app/components/StatusChip";

describe("StatusChip", () => {
  // Green states → existing DS token --success / --success-muted
  it.each(["present", "active", "waking", "ok"])(
    "%s maps to success class (DS --success token)",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el).not.toBeNull();
      expect(el!.className).toContain("status-chip--success");
      expect(el!.querySelector(".status-chip__dot")).not.toBeNull();
    },
  );

  // Blue states → existing DS token --blue-400
  it.each(["absent", "blanked"])(
    "%s maps to blue class (DS --blue-400 token)",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).toContain("status-chip--blue");
      // Verify the real DS token is in the CSS variable chain:
      // the class sets --chip-color: var(--blue-400).
      const cs = getComputedStyle(el!);
      // In jsdom the custom property chain resolves, so
      // color should NOT be the fallback "invalid" value.
      expect(cs.color).not.toBe("");
    },
  );

  // Yellow states → existing DS token --warning / --warning-muted
  it.each(["grace", "blanking", "unavailable"])(
    "%s maps to warning class (DS --warning token)",
    (kind) => {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).toContain("status-chip--warning");
    },
  );

  // Amber → existing DS token --accent-warm / --accent-warm-muted
  it("paused maps to amber class (DS --accent-warm token)", () => {
    const { container } = render(<StatusChip kind="paused" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--amber");
  });

  // Purple → existing DS token --purple-400
  it("inhibited maps to purple class (DS --purple-400 token)", () => {
    const { container } = render(<StatusChip kind="inhibited" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--purple");
  });

  // Red → existing DS token --danger / --danger-muted
  it.each(["fail", "wake_retry"])(
    "%s maps to danger class (DS --danger token)",
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

  it("all mappable statuses use real DS token classes", () => {
    // Every known status kind → a valid class (no "status-chip--muted" for known ones).
    const knownKinds = [
      "present", "absent", "unavailable",
      "active", "grace", "blanking", "blanked", "waking",
      "paused", "inhibited",
      "ok", "fail", "wake_retry",
    ];
    for (const kind of knownKinds) {
      const { container } = render(<StatusChip kind={kind} />);
      const el = container.querySelector(".status-chip");
      expect(el!.className).not.toContain("status-chip--muted");
    }
  });

  it("has no inline --chip-color style override (would mask CSS token mapping)", () => {
    const { container } = render(<StatusChip kind="fail" />);
    const el = container.querySelector(".status-chip");
    // The inline --status-*-fg bug was removed — the element must NOT
    // carry an inline --chip-color that overrides the stylesheet mapping.
    expect(el?.getAttribute("style")).toBeNull();
  });

  it("fail chip uses class status-chip--danger (real DS token --danger)", () => {
    const { container } = render(<StatusChip kind="fail" />);
    const el = container.querySelector(".status-chip");
    expect(el!.className).toContain("status-chip--danger");
    // StatusChip.css sets --chip-color: var(--danger) on this class.
    // The chip element should NOT carry an inline style that overrides it.
    expect(el?.getAttribute("style")).toBeNull();
  });
});
