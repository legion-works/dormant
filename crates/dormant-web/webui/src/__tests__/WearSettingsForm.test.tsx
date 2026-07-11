/**
 * Settings-form pins for the wear (panel-exposure) config surface:
 *   - WearSection: all 10 `wear.*` keys, each round-tripping through
 *     buildPatches() at the exact `wear.<key>` path.
 *   - DisplaysSection: `panel_type` select, exact `displays.<id>.panel_type`
 *     patch path.
 *   - ScreensaverEditor: `shift_px` / `shift_interval`, exact
 *     `displays.<id>.screensaver.shift_*` patch paths.
 *
 * Sections are rendered directly with a real PatchStore (mirroring the
 * "DisplaysSection — mode switch" pattern in ConfigForm.test.tsx) so
 * patch assertions are precise without going through the full apply flow.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import WearSection from "../app/config/WearSection";
import DisplaysSection from "../app/config/DisplaysSection";
import ScreensaverEditor from "../app/config/ScreensaverEditor";
import type { DisplayConfig, ScreensaverConfig } from "../api/types";

afterEach(() => cleanup());

// ── WearSection — all 10 wear.* keys ──

const WEAR_INVENTORY: Record<string, unknown> = {
  enabled: true,
  sample_interval: "60s",
  persist_interval: "300s",
  read_timeout: "2s",
  grid_rows: 9,
  grid_cols: 16,
  fallback_brightness: 0.5,
  screensaver_factor: 0.35,
  short_cycle_dwell: "600s",
  advisory_after: "96h",
};

describe("WearSection — settings form", () => {
  it("renders all 10 wear.* keys with a labelled control", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={WEAR_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    for (const key of Object.keys(WEAR_INVENTORY)) {
      expect(screen.getByLabelText(key)).toBeInTheDocument();
    }
  });

  it("renders enabled as a checkbox and editing it emits the exact wear.enabled patch", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={WEAR_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const checkbox = screen.getByLabelText("enabled") as HTMLInputElement;
    expect(checkbox.type).toBe("checkbox");
    expect(checkbox.checked).toBe(true);

    fireEvent.click(checkbox);

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["wear", "enabled"],
      value: false,
    });
  });

  it("editing every remaining wear.* field emits its exact patch path", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={WEAR_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("sample_interval"), { target: { value: "90s" } });
    fireEvent.change(screen.getByLabelText("persist_interval"), { target: { value: "10m" } });
    fireEvent.change(screen.getByLabelText("read_timeout"), { target: { value: "3s" } });
    fireEvent.change(screen.getByLabelText("grid_rows"), { target: { value: "12" } });
    fireEvent.change(screen.getByLabelText("grid_cols"), { target: { value: "20" } });
    fireEvent.change(screen.getByLabelText("fallback_brightness"), { target: { value: "0.7" } });
    fireEvent.change(screen.getByLabelText("screensaver_factor"), { target: { value: "0.4" } });
    fireEvent.change(screen.getByLabelText("short_cycle_dwell"), { target: { value: "900s" } });
    fireEvent.change(screen.getByLabelText("advisory_after"), { target: { value: "48h" } });

    const patches = store.buildPatches();
    const paths = patches.map((p) => p.path.join("."));

    expect(paths).toContain("wear.sample_interval");
    expect(paths).toContain("wear.persist_interval");
    expect(paths).toContain("wear.read_timeout");
    expect(paths).toContain("wear.grid_rows");
    expect(paths).toContain("wear.grid_cols");
    expect(paths).toContain("wear.fallback_brightness");
    expect(paths).toContain("wear.screensaver_factor");
    expect(paths).toContain("wear.short_cycle_dwell");
    expect(paths).toContain("wear.advisory_after");

    expect(patches).toContainEqual({ op: "set", path: ["wear", "grid_rows"], value: 12 });
    expect(patches).toContainEqual({
      op: "set",
      path: ["wear", "fallback_brightness"],
      value: 0.7,
    });
  });
});

// ── DisplaysSection — panel_type ──

const DISPLAY_WITH_PANEL_TYPE: Record<string, DisplayConfig> = {
  tv: { controllers: ["ddcci"], blank_mode: "power_off", panel_type: "woled" },
};

describe("DisplaysSection — panel_type", () => {
  it("renders a panel_type select with woled/qd-oled/unknown options, defaulted to the config value", () => {
    const store = createPatchStore();
    render(
      <DisplaysSection
        displays={DISPLAY_WITH_PANEL_TYPE}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const select = screen.getByLabelText("panel_type") as HTMLSelectElement;
    expect(select.tagName).toBe("SELECT");
    expect(Array.from(select.options).map((o) => o.value)).toEqual([
      "woled",
      "qd-oled",
      "unknown",
    ]);
    expect(select.value).toBe("woled");
  });

  it("editing panel_type emits the exact displays.<id>.panel_type patch", () => {
    const store = createPatchStore();
    render(
      <DisplaysSection
        displays={DISPLAY_WITH_PANEL_TYPE}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("panel_type"), { target: { value: "qd-oled" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "panel_type"],
      value: "qd-oled",
    });
  });
});

// ── ScreensaverEditor — shift_px / shift_interval ──

const SCREENSAVER: ScreensaverConfig = {
  trigger: "vacancy",
  audio: false,
  source: [],
  shift_px: 2,
  shift_interval: "120s",
};

describe("ScreensaverEditor — shift_px / shift_interval", () => {
  it("renders shift_px (number) and shift_interval (duration) fields with the config defaults", () => {
    const store = createPatchStore();
    render(
      <ScreensaverEditor
        screensaver={SCREENSAVER}
        displayId="tv"
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const shiftPx = screen.getByLabelText("shift_px") as HTMLInputElement;
    expect(shiftPx.type).toBe("number");
    expect(shiftPx.value).toBe("2");

    const shiftInterval = screen.getByLabelText("shift_interval") as HTMLInputElement;
    expect(shiftInterval.type).toBe("text");
    expect(shiftInterval.value).toBe("120s");
  });

  it("editing shift_px emits the exact displays.<id>.screensaver.shift_px patch", () => {
    const store = createPatchStore();
    render(
      <ScreensaverEditor
        screensaver={SCREENSAVER}
        displayId="tv"
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("shift_px"), { target: { value: "5" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "screensaver", "shift_px"],
      value: 5,
    });
  });

  it("editing shift_interval emits the exact displays.<id>.screensaver.shift_interval patch", () => {
    const store = createPatchStore();
    render(
      <ScreensaverEditor
        screensaver={SCREENSAVER}
        displayId="tv"
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("shift_interval"), { target: { value: "90s" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "screensaver", "shift_interval"],
      value: "90s",
    });
  });
});
