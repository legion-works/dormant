/**
 * eventFormat tests — badge + message formatters for wear events, and
 * the unknown-tag fallthrough both formatters share with every other
 * DaemonEvent variant.
 */
import { describe, it, expect } from "vitest";
import { badgeForEvent, messageForEvent } from "../app/views/eventFormat";
import type { DaemonEvent } from "../api/types";

describe("eventFormat — wear_snapshot", () => {
  it("badgeForEvent labels it 'wear'", () => {
    const ev: DaemonEvent = {
      event: "wear_snapshot",
      display: "d1",
      total_on_hours: 12.34,
      sample_count: 7,
    };
    expect(badgeForEvent(ev).label).toBe("wear");
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
  it("badgeForEvent labels it 'advisory'", () => {
    const ev: DaemonEvent = {
      event: "compensation_advisory",
      display: "d1",
      hours_since_long_dwell: 100,
    };
    expect(badgeForEvent(ev).label).toBe("advisory");
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
