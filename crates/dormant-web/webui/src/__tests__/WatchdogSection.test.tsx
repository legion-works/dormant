/**
 * WatchdogSection tests — settings-form pins for the `[watchdog]` config
 * surface (mirrors NotificationsSection.test.tsx's pattern): all three
 * `watchdog.*` keys, each round-tripping through buildPatches() at the
 * exact `watchdog.<key>` path.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import WatchdogSection from "../app/config/WatchdogSection";

afterEach(() => cleanup());

const WATCHDOG_INVENTORY: Record<string, unknown> = {
  lkg_enabled: true,
  lkg_rollback_enabled: true,
  stability_window: "300s",
};

describe("WatchdogSection — settings form", () => {
  it("renders all 3 watchdog.* keys with a labelled control", () => {
    const store = createPatchStore();
    render(
      <WatchdogSection
        watchdog={WATCHDOG_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    for (const key of Object.keys(WATCHDOG_INVENTORY)) {
      expect(screen.getByLabelText(key)).toBeInTheDocument();
    }
  });

  it("renders lkg_enabled as a checkbox and editing it emits the exact watchdog.lkg_enabled patch", () => {
    const store = createPatchStore();
    render(
      <WatchdogSection
        watchdog={WATCHDOG_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const checkbox = screen.getByLabelText("lkg_enabled") as HTMLInputElement;
    expect(checkbox.type).toBe("checkbox");
    expect(checkbox.checked).toBe(true);

    fireEvent.click(checkbox);

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["watchdog", "lkg_enabled"],
      value: false,
    });
  });

  it("renders lkg_rollback_enabled as a checkbox and editing it emits the exact watchdog.lkg_rollback_enabled patch", () => {
    const store = createPatchStore();
    render(
      <WatchdogSection
        watchdog={WATCHDOG_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const checkbox = screen.getByLabelText("lkg_rollback_enabled") as HTMLInputElement;
    expect(checkbox.type).toBe("checkbox");
    expect(checkbox.checked).toBe(true);

    fireEvent.click(checkbox);

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["watchdog", "lkg_rollback_enabled"],
      value: false,
    });
  });

  it("editing stability_window (duration) emits the exact watchdog.stability_window patch", () => {
    const store = createPatchStore();
    render(
      <WatchdogSection
        watchdog={WATCHDOG_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("stability_window"), { target: { value: "60s" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["watchdog", "stability_window"],
      value: "60s",
    });
  });

  it("emits no patch other than the exact watchdog.<key> path when editing each field", () => {
    const store = createPatchStore();
    render(
      <WatchdogSection
        watchdog={WATCHDOG_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.click(screen.getByLabelText("lkg_enabled"));
    fireEvent.click(screen.getByLabelText("lkg_rollback_enabled"));
    fireEvent.change(screen.getByLabelText("stability_window"), { target: { value: "60s" } });

    const patches = store.buildPatches();
    expect(patches).toHaveLength(3);
    const paths = patches.filter((p) => "path" in p).map((p) => p.path.join("."));
    expect(paths.sort()).toEqual(
      ["watchdog.lkg_enabled", "watchdog.lkg_rollback_enabled", "watchdog.stability_window"].sort(),
    );
  });

  it("renders nothing when watchdog is undefined (legacy fixture)", () => {
    const store = createPatchStore();
    const { container } = render(
      <WatchdogSection
        watchdog={undefined}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(container.firstChild).toBeNull();
  });
});
