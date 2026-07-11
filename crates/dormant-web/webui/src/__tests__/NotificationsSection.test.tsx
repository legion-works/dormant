/**
 * NotificationsSection tests — settings-form pins for the
 * `[notifications]` config surface (mirrors WearSettingsForm.test.tsx's
 * WearSection pattern): all four `notifications.*` keys, each
 * round-tripping through buildPatches() at the exact
 * `notifications.<key>` path.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import NotificationsSection from "../app/config/NotificationsSection";

afterEach(() => cleanup());

const NOTIFICATIONS_INVENTORY: Record<string, unknown> = {
  enabled: true,
  wake_attempt_threshold: 3,
  cooldown: "15m",
  notify_recovery: true,
};

describe("NotificationsSection — settings form", () => {
  it("renders all 4 notifications.* keys with a labelled control", () => {
    const store = createPatchStore();
    render(
      <NotificationsSection
        notifications={NOTIFICATIONS_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    for (const key of Object.keys(NOTIFICATIONS_INVENTORY)) {
      expect(screen.getByLabelText(key)).toBeInTheDocument();
    }
  });

  it("renders enabled as a checkbox and editing it emits the exact notifications.enabled patch", () => {
    const store = createPatchStore();
    render(
      <NotificationsSection
        notifications={NOTIFICATIONS_INVENTORY}
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
      path: ["notifications", "enabled"],
      value: false,
    });
  });

  it("editing wake_attempt_threshold emits the exact notifications.wake_attempt_threshold patch", () => {
    const store = createPatchStore();
    render(
      <NotificationsSection
        notifications={NOTIFICATIONS_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("wake_attempt_threshold"), { target: { value: "5" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["notifications", "wake_attempt_threshold"],
      value: 5,
    });
  });

  it("editing cooldown (duration) emits the exact notifications.cooldown patch", () => {
    const store = createPatchStore();
    render(
      <NotificationsSection
        notifications={NOTIFICATIONS_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("cooldown"), { target: { value: "30m" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["notifications", "cooldown"],
      value: "30m",
    });
  });

  it("editing notify_recovery emits the exact notifications.notify_recovery patch", () => {
    const store = createPatchStore();
    render(
      <NotificationsSection
        notifications={NOTIFICATIONS_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const checkbox = screen.getByLabelText("notify_recovery") as HTMLInputElement;
    fireEvent.click(checkbox);

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["notifications", "notify_recovery"],
      value: false,
    });
  });

  it("renders nothing when notifications is undefined (legacy fixture)", () => {
    const store = createPatchStore();
    const { container } = render(
      <NotificationsSection
        notifications={undefined}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(container.firstChild).toBeNull();
  });
});
