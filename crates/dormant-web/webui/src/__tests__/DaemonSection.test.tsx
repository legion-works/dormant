/**
 * DaemonSection posture tests — M2/S2 regression guards.
 *
 * Covers the LAN posture prose enumeration of write routes.
 * The route list is hand-maintained; this test catches drift.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import DaemonSection from "../app/config/DaemonSection";
import type { PatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
});

function mockStore(): PatchStore {
  return {
    trackEdit() {},
    trackRemove() {},
    getEdit() { return undefined; },
    isRemoved() { return false; },
    trackCreate() {},
    trackDelete() {},
    buildPatches() { return []; },
    isLocked() { return false; },
    reset() {},
  };
}

/** All known POST/write routes under `/api` — authoritative inventory
 *  from `crates/dormant-web/src/server.rs:105-128` (route_post! registrations). */
const KNOWN_WRITE_ROUTES = [
  "config apply",
  "blank",
  "wake",
  "switch",
  "push",
  "pause",
  "resume",
  "reload",
  "doctor",
  "emergency wake",
  "pairing",
  "doctor exercise",
];

describe("DaemonSection posture", () => {
  it("enumerates all known write routes in LAN posture prose", () => {
    render(
      <DaemonSection
        daemon={{
          web_bind: "0.0.0.0",
          web_allow_nonloopback: true,
          web_port: 8080,
          startup_holdoff: "30s",
          stale_sensor_timeout: "300s",
          log_level: "info",
          idle_time_unit: "auto",
          idle_source: "auto",
          reload_debounce: "500ms",
          generation_barrier_ack_timeout: "2s",
          entity_crud_enabled: true,
          pairing_enabled: true,
          hook_edit_enabled: false,
          pair_timeout: "120s",
          doctor_wake_settle: "2s",
          macos_idle_frozen_polls: 3,
          macos_idle_sanity_cap: "60s",
          macos_idle_startup_grace: "5s",
        }}
        store={mockStore()}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    // The posture card should render "LAN" chip.
    expect(screen.getByText("LAN")).toBeInTheDocument();

    // The prose body text — the second <span> inside the posture card.
    // The first <span> is the "LAN" chip label.
    const proseEl = document.querySelector(".cf-posture-card--lan span + span");
    const prose = proseEl?.textContent ?? "";
    for (const route of KNOWN_WRITE_ROUTES) {
      expect(prose.toLowerCase()).toContain(route.toLowerCase());
    }
  });
});
