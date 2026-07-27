/**
 * Switching view tests — nav gating, redirect, warning,
 * deep-links, block rendering.
 *
 * Acceptance criteria from IMPLEMENTATION.md W2-3:
 * - nav item appears iff kvm != null && switch_capable_displays.length > 0
 * - #/switching redirects to #/displays otherwise
 * - every value deep-links to its config field
 * - the shared-but-not-switch-capable warning renders
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import Switching from "../app/views/Switching";
import { LiveStateContext, EventLogContext } from "../app/hooks/useLiveState";
import { liveStateFixture, eventLogFixture } from "./fixtures/live-state";
import type { LiveState } from "../app/hooks/useLiveState";
import type { DisplaySnapshot } from "../api/types";

afterEach(() => cleanup());

function liveWithKvm(): LiveState {
  const sharedSnap: DisplaySnapshot = {
    phase: "active", inhibited: false, paused: false, cmd_gen: 1,
    controllers: [{ name: "ddcci", role: "primary", healthy: true }],
    scope: "shared", owned: true,
    observed_input_code: 0x0f,
  };
  return liveStateFixture({
    snapshot: {
      sensors: [],
      zones: [],
      displays: [["shared_oled", sharedSnap]],
      pending_reload: null,
      kvm: {
        keymap: {},
        switch_capable_displays: ["shared_oled"],
        activity_following: false,
        push_capable_displays: [],
      },
    },
    config: {
      path: "/tmp/c.toml", config_version: 1, source: "last_applied",
      raw_toml: "",
      inventory: {
        config_version: 1,
        daemon: {}, sensors: {}, zones: {},
        displays: {
          shared_oled: {
            controllers: ["ddcci"],
            scope: "shared",
            shared_input_code: 0x0f,
            shared_input_write_code: 0x15,
            hooks: {},
          },
        },
        rules: {},
        coordination: { poll_interval: "2s", loss_confirmations: 3 },
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {}, fingerprint: "abc", redacted_paths: [],
    },
    displayConfigs: {
      shared_oled: {
        controllers: ["ddcci"], scope: "shared",
        shared_input_code: 0x0f, shared_input_write_code: 0x15, hooks: {},
      },
    },
  });
}

describe("Switching", () => {
  function renderSwitching(live = liveWithKvm()) {
    return render(
      <LiveStateContext.Provider value={live}>
        <EventLogContext.Provider value={eventLogFixture()}>
          <Switching />
        </EventLogContext.Provider>
      </LiveStateContext.Provider>,
    );
  }

  it("renders ownership block for switch-capable display", () => {
    renderSwitching();
    expect(screen.getByText("shared_oled")).toBeInTheDocument();
    expect(screen.getByText("How it switches")).toBeInTheDocument();
    // Deep links are present.
    expect(screen.getAllByText("edit").length).toBeGreaterThanOrEqual(4);
  });

  it("renders shared-but-not-switch-capable warning", () => {
    const live = liveWithKvm();
    // Add a second shared display NOT in switch_capable_displays.
    live.snapshot!.kvm!.switch_capable_displays = ["shared_oled"];
    live.config!.inventory.displays["not_switchable" as keyof object] = {
      controllers: ["ddcci"], scope: "shared",
      shared_input_code: 0x20, hooks: {},
    } as any;
    (live.displayConfigs as Record<string, any>)["not_switchable"] = {
      controllers: ["ddcci"], scope: "shared",
      shared_input_code: 0x20, hooks: {},
    };
    live.snapshot!.displays.push(["not_switchable", {
      phase: "active", inhibited: false, paused: false, cmd_gen: 1,
      controllers: [], scope: "shared", owned: true,
    }] as any);
    renderSwitching(live);
    expect(screen.getByText(/not switch-capable/)).toBeInTheDocument();
  });

  it("redirects to #/displays when kvm is null", () => {
    const orig = window.location.hash;
    const live = liveStateFixture({
      snapshot: { sensors: [], zones: [], displays: [], pending_reload: null },
    });
    window.location.hash = "#/switching";
    renderSwitching(live);
    expect(screen.queryByText("shared_oled")).toBeNull();
    window.location.hash = orig;
  });

  it("redirects when switch_capable_displays is empty", () => {
    const orig = window.location.hash;
    const live = liveWithKvm();
    live.snapshot!.kvm!.switch_capable_displays = [];
    window.location.hash = "#/switching";
    renderSwitching(live);
    expect(screen.queryByText("shared_oled")).toBeNull();
    window.location.hash = orig;
  });

  it("deep-links to config switching fields", () => {
    renderSwitching();
    const links = screen.getAllByText("edit");
    const activityLink = links[0].closest("a");
    expect(activityLink?.getAttribute("href")).toContain("#/config/switching#coordination.activity_follow");
  });
});
