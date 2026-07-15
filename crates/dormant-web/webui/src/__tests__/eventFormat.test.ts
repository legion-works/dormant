/**
 * eventFormat tests — badge + message formatters for wear events, and
 * the unknown-tag fallthrough both formatters share with every other
 * DaemonEvent variant.
 */
import { describe, it, expect } from "vitest";
import { badgeForEvent, messageForEvent } from "../app/views/eventFormat";
import type { DaemonEvent } from "../api/types";

describe("eventFormat — wear_snapshot", () => {
  it("badgeForEvent labels it 'wear_snapshot'", () => {
    const ev: DaemonEvent = {
      event: "wear_snapshot",
      display: "d1",
      total_on_hours: 12.34,
      sample_count: 7,
    };
    expect(badgeForEvent(ev).label).toBe("wear_snapshot");
  });

  it("messageForEvent reports on-hours and sample count", () => {
    const ev: DaemonEvent = {
      event: "wear_snapshot",
      display: "d1",
      total_on_hours: 12.34,
      sample_count: 7,
    };
    expect(messageForEvent(ev)).toBe("d1: 12.3h total on-time (7 samples)");
  });
});

describe("eventFormat — compensation_advisory", () => {
  it("badgeForEvent labels it 'compensation_advisory'", () => {
    const ev: DaemonEvent = {
      event: "compensation_advisory",
      display: "d1",
      hours_since_long_dwell: 100,
    };
    expect(badgeForEvent(ev).label).toBe("compensation_advisory");
  });

  it("messageForEvent reports days since long-dwell worded 'no long standby window in N days'", () => {
    const ev: DaemonEvent = {
      event: "compensation_advisory",
      display: "d1",
      hours_since_long_dwell: 100,
    };
    expect(messageForEvent(ev)).toBe("d1: no long standby window in 4 days");
  });
});

describe("eventFormat — blank_failure", () => {
  it("badgeForEvent labels it with a danger badge", () => {
    const ev: DaemonEvent = {
      event: "blank_failure",
      display: "d1",
      controller: "ddcci",
      detail: "E_TIMEOUT: no ack",
    };
    const badge = badgeForEvent(ev);
    expect(badge.label).toBe("blank_failure");
    expect(badge.color).toBe("var(--danger)");
  });

  it("messageForEvent reports the display, controller, and detail", () => {
    const ev: DaemonEvent = {
      event: "blank_failure",
      display: "d1",
      controller: "ddcci",
      detail: "E_TIMEOUT: no ack",
    };
    const msg = messageForEvent(ev);
    expect(msg).toContain("d1");
    expect(msg).toContain("ddcci");
    expect(msg).toContain("E_TIMEOUT: no ack");
  });
});

describe("eventFormat — blank_recovered", () => {
  it("badgeForEvent labels it 'blank_recovered'", () => {
    const ev: DaemonEvent = { event: "blank_recovered", display: "d1" };
    expect(badgeForEvent(ev).label).toBe("blank_recovered");
  });

  it("messageForEvent reports the display recovered", () => {
    const ev: DaemonEvent = { event: "blank_recovered", display: "d1" };
    expect(messageForEvent(ev)).toBe("d1: blank recovered");
  });
});

describe("eventFormat — wake_recovered", () => {
  it("badgeForEvent labels it 'wake_recovered'", () => {
    const ev: DaemonEvent = { event: "wake_recovered", display: "d1", attempts: 3 };
    expect(badgeForEvent(ev).label).toBe("wake_recovered");
  });

  it("messageForEvent reports the display and attempt count", () => {
    const ev: DaemonEvent = { event: "wake_recovered", display: "d1", attempts: 3 };
    expect(messageForEvent(ev)).toBe("d1: wake recovered after 3 attempts");
  });

  it("messageForEvent singularizes a single attempt", () => {
    const ev: DaemonEvent = { event: "wake_recovered", display: "d1", attempts: 1 };
    expect(messageForEvent(ev)).toBe("d1: wake recovered after 1 attempt");
  });
});

describe("eventFormat — exact badge map (v2 handoff)", () => {
  it.each([
    [{ event: "sensor_changed", sensor: "desk", state: "present" }, "sensor_changed", "var(--blue-400)"],
    [{ event: "zone_changed", zone: "office", present: true, cause: "radar" }, "zone_changed", "var(--success)"],
    [{ event: "display_phase", display: "main", phase: "active", cause: "presence" }, "display_phase", "var(--text-muted)"],
    [{ event: "wake_retry", display: "main", attempt: 2 }, "wake_retry", "var(--danger)"],
    [{ event: "config_reloaded" }, "config_reloaded", "var(--accent-warm)"],
    [{ event: "config_reload_rejected", detail: "bad config" }, "config_reload_rejected", "var(--danger)"],
    [{ event: "wear_snapshot", display: "main", total_on_hours: 12, sample_count: 5 }, "wear_snapshot", "var(--purple-400)"],
    [{ event: "compensation_advisory", display: "main", hours_since_long_dwell: 96 }, "compensation_advisory", "var(--warning)"],
    [{ event: "blank_failure", display: "main", controller: "ddcci", detail: "timeout" }, "blank_failure", "var(--danger)"],
    [{ event: "blank_recovered", display: "main" }, "blank_recovered", "var(--success)"],
    [{ event: "wake_recovered", display: "main", attempts: 2 }, "wake_recovered", "var(--success)"],
  ] as const)("maps %o to exact label %s", (event, label, color) => {
    expect(badgeForEvent(event as DaemonEvent)).toMatchObject({ label, color });
  });
});

describe("eventFormat — unknown tag fallthrough (both formatters)", () => {
  it("badgeForEvent falls through to the default arm using the raw tag as the label", () => {
    const ev = { event: "some_future_tag" } as unknown as DaemonEvent;
    const badge = badgeForEvent(ev);
    expect(badge.label).toBe("some_future_tag");
  });

  it("messageForEvent falls through to the default arm (JSON dump)", () => {
    const ev = { event: "some_future_tag", extra: 1 } as unknown as DaemonEvent;
    expect(messageForEvent(ev)).toBe(JSON.stringify(ev));
  });
});
