/**
 * AudioSection tests — settings-form pins for the `[audio]` config
 * surface (mirrors NotificationsSection.test.tsx / WearSettingsForm.test.tsx
 * patterns): all six `audio.*` keys render, five editable keys round-trip
 * through buildPatches() at the exact `audio.<key>` path, and
 * `pw_dump_command` is locked read-only per the T7 security fold
 * (spec §6#10) — NO patch-path emission for it, ever.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import AudioSection from "../app/config/AudioSection";

afterEach(() => cleanup());

const AUDIO_INVENTORY: Record<string, unknown> = {
  poll_interval: "5s",
  min_active: "3s",
  call_roles: ["Communication"],
  playback_roles: null,
  capture_is_call: false,
  pw_dump_command: "pw-dump",
};

describe("AudioSection — settings form", () => {
  it("renders all 6 audio.* keys with a labelled control", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByLabelText("poll_interval")).toBeInTheDocument();
    expect(screen.getByLabelText("min_active")).toBeInTheDocument();
    expect(screen.getByLabelText("call_roles")).toBeInTheDocument();
    // playback_roles is unset in the fixture — only the enable-toggle renders.
    expect(screen.getByLabelText(/playback_roles/)).toBeInTheDocument();
    expect(screen.getByLabelText("capture_is_call")).toBeInTheDocument();
    // pw_dump_command is a locked read-only value chip, not an <input> —
    // it has a label element but no associated form control.
    expect(screen.getByText("pw_dump_command")).toBeInTheDocument();
    expect(screen.getByText("pw-dump")).toBeInTheDocument();
  });

  it("editing poll_interval (duration) emits the exact audio.poll_interval patch", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("poll_interval"), { target: { value: "10s" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "poll_interval"],
      value: "10s",
    });
  });

  it("editing min_active (duration) emits the exact audio.min_active patch", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("min_active"), { target: { value: "8s" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "min_active"],
      value: "8s",
    });
  });

  it("editing capture_is_call (bool) emits the exact audio.capture_is_call patch", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const checkbox = screen.getByLabelText("capture_is_call") as HTMLInputElement;
    expect(checkbox.type).toBe("checkbox");
    expect(checkbox.checked).toBe(false);

    fireEvent.click(checkbox);

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "capture_is_call"],
      value: true,
    });
  });

  it("adding an entry to call_roles (string list) emits the exact audio.call_roles patch", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.change(screen.getByLabelText("call_roles"), { target: { value: "Notification" } });
    fireEvent.click(screen.getByLabelText("Add call_roles"));

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "call_roles"],
      value: ["Communication", "Notification"],
    });
  });

  it("pw_dump_command renders disabled/locked with the v1 tooltip and never appears in buildPatches", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByText("pw-dump")).toBeInTheDocument();
    expect(screen.getByTitle("not editable in v1 — feature 05 will gate this")).toBeInTheDocument();

    // No input control should be reachable/editable for this key.
    const el = screen.queryByLabelText("pw_dump_command") as HTMLInputElement | null;
    if (el) {
      expect(el.disabled).toBe(true);
      fireEvent.change(el, { target: { value: "rm -rf /" } });
    }

    const patches = store.buildPatches();
    expect(patches.some((p) => "path" in p && p.path.join(".") === "audio.pw_dump_command")).toBe(false);
  });

  it("playback_roles unset renders as an 'any role' state and emits no patch until explicitly set", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const toggle = screen.getByLabelText(/playback_roles/) as HTMLInputElement;
    expect(toggle.type).toBe("checkbox");
    expect(toggle.checked).toBe(false);
    expect(screen.getByText(/any role/)).toBeInTheDocument();

    expect(store.buildPatches()).toEqual([]);
  });

  it("setting playback_roles (toggle on + add a role) emits an audio.playback_roles set patch", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.click(screen.getByLabelText(/playback_roles/));
    fireEvent.change(screen.getByLabelText("playback_roles"), { target: { value: "Movie" } });
    fireEvent.click(screen.getByLabelText("Add playback_roles"));

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "playback_roles"],
      value: ["Movie"],
    });
  });

  it("checking playback_roles with no roles yet tracks nothing (F16 guard); adding a role then emits the set", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    fireEvent.click(screen.getByLabelText(/playback_roles/)); // check, no roles added yet

    expect(
      store.buildPatches().some((p) => "path" in p && p.path.join(".") === "audio.playback_roles"),
    ).toBe(false);

    fireEvent.change(screen.getByLabelText("playback_roles"), { target: { value: "Movie" } });
    fireEvent.click(screen.getByLabelText("Add playback_roles"));

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["audio", "playback_roles"],
      value: ["Movie"],
    });
  });

  it("setting then clearing playback_roles back to unset emits a remove (not a set)", () => {
    const store = createPatchStore();
    render(
      <AudioSection
        audio={AUDIO_INVENTORY}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const toggle = screen.getByLabelText(/playback_roles/) as HTMLInputElement;
    fireEvent.click(toggle); // set
    fireEvent.change(screen.getByLabelText("playback_roles"), { target: { value: "Movie" } });
    fireEvent.click(screen.getByLabelText("Add playback_roles"));
    fireEvent.click(toggle); // back to unset

    const patches = store.buildPatches();
    expect(patches).toContainEqual({ op: "remove", path: ["audio", "playback_roles"] });
    expect(patches.some((p) => p.op === "set" && p.path.join(".") === "audio.playback_roles")).toBe(false);
  });

  it("renders nothing when audio is undefined (legacy fixture)", () => {
    const store = createPatchStore();
    const { container } = render(
      <AudioSection
        audio={undefined}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(container.firstChild).toBeNull();
  });
});
