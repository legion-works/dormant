/**
 * DisplaysSection — CRUD affordances (spec §7, config-crud-wizard T6).
 *
 * Covers: Add button gated by entity_crud_enabled; create form emits
 * `create_entity` via the store; per-card delete with a
 * references-warning confirm naming referencing rules.
 *
 * DisplaySamplingEditor — per-display compositor-sampling fields
 * (compositor_output + the [displays.<id>.sampling] table: expected_source,
 * source_poll_interval, stream_mode, watched_apps). Stream overrides fall
 * back to the global wear.active_sampling.stream_mode when unset; the
 * editor surfaces this as an "Inherit global" select choice.
 * `watched_apps` is the port-8001 Tizen app id catalog the source gate
 * probes each poll cycle (issue #232) — empty disables the app-visibility
 * check and reverts to the daemon-shipped seed catalog.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, cleanup, act } from "@testing-library/react";
import DisplaysSection from "../app/config/DisplaysSection";
import { createPatchStore } from "../app/config/patch";
import type { DisplayConfig, RuleConfig } from "../api/types";

afterEach(() => cleanup());

const DISPLAYS: Record<string, DisplayConfig> = {
  "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" },
};

const RULES: Record<string, RuleConfig> = {
  "office-rule": { zone: "office", displays: ["aoc-main"] },
};

function renderSection(overrides: Partial<Parameters<typeof DisplaysSection>[0]> = {}) {
  const store = createPatchStore();
  const props = {
    displays: DISPLAYS,
    store,
    redactedPaths: [] as string[][],
    onDirty: () => {},
    fieldErrors: {},
    entityCrudEnabled: true,
    rules: RULES,
    ...overrides,
  };
  render(<DisplaysSection {...props} />);
  return { store, props };
}

describe("DisplaysSection — Add affordance", () => {
  it("display_editor_round_trips_shared_scope_and_input_code", () => {
    const { store } = renderSection({
      displays: { "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off", scope: "shared", shared_input_code: 15 } },
    });

    expect(screen.getByLabelText("scope")).toHaveValue("shared");
    expect(screen.getByLabelText(/shared_input_code/)).toHaveValue(15);
    // Hex echo: label text includes "0x0f (15)" when value is 15.
    expect(screen.getByLabelText(/shared_input_code.*0x0f.*15/)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText(/shared_input_code/), { target: { value: "16" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "aoc-main", "shared_input_code"],
      value: 16,
    });
  });

  it("shows an Add button when entity_crud_enabled", () => {
    renderSection();
    expect(screen.getByRole("button", { name: /add display/i })).toBeInTheDocument();
  });

  it("hides the Add button when entity_crud_enabled is false", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByRole("button", { name: /add display/i })).not.toBeInTheDocument();
  });

  it("creating a display via the form emits an exact create_entity patch, never blank_command/wake_command", () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /add display/i }));

    fireEvent.change(screen.getByLabelText("id"), { target: { value: "new-tv" } });
    fireEvent.click(screen.getByLabelText("controllers: samsung-tizen"));
    fireEvent.change(screen.getByLabelText("host"), { target: { value: "192.0.2.50" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toMatchObject({ op: "create_entity", collection: "displays", id: "new-tv" });
    const value = (patches[0] as { value: Record<string, unknown> }).value;
    expect(value.controllers).toEqual(["samsung-tizen"]);
    expect(value.host).toBe("192.0.2.50");
    expect(value).not.toHaveProperty("blank_command");
    expect(value).not.toHaveProperty("wake_command");
  });
});

describe("DisplaysSection — pairing wizard hand-off (spec §8.3)", () => {
  it("createPrefill auto-opens the create form pre-filled with host + controllers", () => {
    render(
      <DisplaysSection
        displays={{}}
        store={createPatchStore()}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
        entityCrudEnabled={true}
        rules={{}}
        createPrefill={{ host: "192.0.2.77", controllers: ["samsung-tizen"] }}
      />,
    );
    expect(screen.getByLabelText("host")).toHaveValue("192.0.2.77");
    expect((screen.getByLabelText("controllers: samsung-tizen") as HTMLInputElement).checked).toBe(true);
  });
});

describe("DisplaysSection — macOS power-off hazard checkbox (issue #126)", () => {
  it("renders the hazard checkbox on a shared-ddcci-power_off display with the warning copy", () => {
    renderSection({
      displays: {
        "aoc-main": {
          controllers: ["ddcci"],
          scope: "shared",
          shared_input_code: 15,
          blank_mode: "power_off",
        },
      },
    });
    const checkbox = screen.getByLabelText(/I have tested physical recovery/i);
    expect(checkbox).toBeInTheDocument();
    expect((checkbox as HTMLInputElement).checked).toBe(false);
    expect(screen.getByText(/unrecoverable/)).toBeInTheDocument();
  });

  it("does not render the hazard checkbox when scope is private", () => {
    renderSection({
      displays: {
        "aoc-main": {
          controllers: ["ddcci"],
          scope: "private",
          blank_mode: "power_off",
        },
      },
    });
    expect(
      screen.queryByLabelText(/I have tested physical recovery/i),
    ).not.toBeInTheDocument();
  });

  it("does not render the hazard checkbox when primary mode is screen_off_audio_on", () => {
    renderSection({
      displays: {
        "aoc-main": {
          controllers: ["ddcci"],
          scope: "shared",
          shared_input_code: 15,
          blank_mode: "screen_off_audio_on",
        },
      },
    });
    expect(
      screen.queryByLabelText(/I have tested physical recovery/i),
    ).not.toBeInTheDocument();
  });

  it("does not render the hazard checkbox when ddcci is a non-first fallback", () => {
    renderSection({
      displays: {
        "aoc-main": {
          controllers: ["macos-gamma-black", "ddcci"],
          scope: "shared",
          shared_input_code: 15,
          blank_mode: "power_off",
        },
      },
    });
    expect(
      screen.queryByLabelText(/I have tested physical recovery/i),
    ).not.toBeInTheDocument();
  });

  it("does not render the hazard checkbox when power_off_opt_in is already true", () => {
    renderSection({
      displays: {
        "aoc-main": {
          controllers: ["ddcci"],
          scope: "shared",
          shared_input_code: 15,
          blank_mode: "power_off",
          power_off_opt_in: true,
        },
      },
    });
    expect(
      screen.queryByLabelText(/I have tested physical recovery/i),
    ).not.toBeInTheDocument();
  });

  it("toggling the checkbox emits a `set` patch on power_off_opt_in", () => {
    const { store } = renderSection({
      displays: {
        "aoc-main": {
          controllers: ["ddcci"],
          scope: "shared",
          shared_input_code: 15,
          blank_mode: "power_off",
        },
      },
    });
    fireEvent.click(
      screen.getByLabelText(/I have tested physical recovery/i),
    );
    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "aoc-main", "power_off_opt_in"],
      value: true,
    });
  });
});

describe("DisplaysSection — Delete affordance", () => {
  it("confirms with the referencing rule before tracking a display delete", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = screen.getByRole("alertdialog", { name: 'Delete display "aoc-main"?' });
    expect(dialog).toHaveTextContent('rule "office-rule"');
    fireEvent.click(screen.getByRole("button", { name: "Delete display" }));
    await waitFor(() => expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "displays", id: "aoc-main" },
    ]));
  });

  it("does not track a display delete when the dialog is cancelled", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // Flush the microtask the async confirm() continuation runs on — see
    // SensorsSection.test.tsx's cancel test for why this is required to
    // actually bite a mutant that ignores `accepted` (C6 precedent). This
    // cancel test did not exist before this task — added to mirror the
    // coverage the other three entity sections have.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(store.buildPatches()).toEqual([]);
  });
});

describe("DisplaysSection — DisplaySamplingEditor (display source-gate fields)", () => {
  const TV_DISPLAY: DisplayConfig = {
    controllers: ["samsung-tizen"],
    host: "192.168.1.50",
    blank_mode: "screen_off_audio_on",
    compositor_output: undefined,
    sampling: undefined,
  };

  function renderTv(overrides: Partial<DisplayConfig> = {}) {
    const cfg: DisplayConfig = { ...TV_DISPLAY, ...overrides };
    const store = createPatchStore();
    render(
      <DisplaysSection
        displays={{ tv: cfg }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
        entityCrudEnabled={false}
        rules={{}}
      />,
    );
    return store;
  }

  it("emits set on displays.tv.compositor_output when typed", () => {
    const store = renderTv();
    fireEvent.change(screen.getByLabelText("compositor_output"), { target: { value: "HDMI-A-1" } });
    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "compositor_output"],
      value: "HDMI-A-1",
    });
  });

  it("emits set on displays.tv.sampling.expected_source when typed", () => {
    const store = renderTv();
    fireEvent.change(screen.getByLabelText("expected_source"), { target: { value: "HDMI4" } });
    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "sampling", "expected_source"],
      value: "HDMI4",
    });
  });

  it("emits set on displays.tv.sampling.source_poll_interval when typed", () => {
    const store = renderTv();
    fireEvent.change(screen.getByLabelText("source_poll_interval"), { target: { value: "15s" } });
    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "sampling", "source_poll_interval"],
      value: "15s",
    });
  });

  it("emits set on displays.tv.sampling.stream_mode when a mode is picked", () => {
    const store = renderTv();
    fireEvent.change(screen.getByLabelText("stream_mode"), { target: { value: "per-tick" } });
    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "tv", "sampling", "stream_mode"],
      value: "per-tick",
    });
  });

  it("emits remove on displays.tv.sampling.stream_mode when the select is cleared back to inherit-global", () => {
    const store = renderTv({ sampling: { expected_source: "HDMI4", source_poll_interval: "15s", stream_mode: "warm" } });
    // First the select is seeded from fetched sampling.stream_mode = "warm";
    // selecting the empty-value "Inherit global" option must remove the
    // override entirely (so the global wear.active_sampling.stream_mode
    // takes effect).
    fireEvent.change(screen.getByLabelText("stream_mode"), { target: { value: "" } });
    expect(store.buildPatches()).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "sampling", "stream_mode"],
    });
  });

  it("renders the stream_mode select with the 'Inherit global' choice as the unset state", () => {
    renderTv({ sampling: { source_poll_interval: "15s" } });
    const select = screen.getByLabelText("stream_mode") as HTMLSelectElement;
    // The select exposes the unset sentinel as the first option; the
    // currently-set value is "" (empty string for the absent Option),
    // which the select renders with the placeholder text.
    expect(select.value).toBe("");
    expect(screen.getByRole("option", { name: /inherit global/i })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "warm" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "per-tick" })).toBeInTheDocument();
  });

  it("seeds expected_source from the fetched sampling table", () => {
    renderTv({ sampling: { expected_source: "HDMI4", source_poll_interval: "15s", stream_mode: "warm" } });
    expect((screen.getByLabelText("expected_source") as HTMLInputElement).value).toBe("HDMI4");
    expect((screen.getByLabelText("source_poll_interval") as HTMLInputElement).value).toBe("15s");
    expect((screen.getByLabelText("stream_mode") as HTMLSelectElement).value).toBe("warm");
  });

  // watched_apps is the port-8001 Tizen app catalog the source gate
  // probes each poll cycle (issue #232). The editor surfaces it as a
  // StringListField with the same `remove`-on-empty invariant the
  // other optional fields use. The seed test below confirms the editor
  // picks up the fetched list verbatim; the add/remove round-trip is
  // exercised through the StringListField component's own test suite.
  it("seeds watched_apps from the fetched sampling table", () => {
    renderTv({
      sampling: {
        expected_source: "HDMI4",
        source_poll_interval: "15s",
        stream_mode: "warm",
        watched_apps: ["111299001912", "3201512006963"],
      },
    });
    // Use the textbox role + matching label — the `aria-label="Remove
    // watched_apps item N"` button also matches the broader regex, so
    // we narrow to the per-item edit input by role.
    const items = screen.getAllByRole("textbox", { name: /watched_apps item/i });
    expect(items).toHaveLength(2);
    expect((items[0] as HTMLInputElement).value).toBe("111299001912");
    expect((items[1] as HTMLInputElement).value).toBe("3201512006963");
  });

  it("emits a remove patch when the last watched_apps entry is removed", () => {
    // Initial seed: a single app id; removing it (last entry) must
    // promote to a `remove` patch so the operator's catalog reverts to
    // the daemon-shipped seed (defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS).
    // The seed is the fail-safe default applied by the serde default fn;
    // removing the explicit list hands the field back to the seed and
    // restores out-of-the-box app detection.
    const store = renderTv({
      sampling: {
        expected_source: "HDMI4",
        source_poll_interval: "15s",
        stream_mode: "warm",
        watched_apps: ["111299001912"],
      },
    });
    fireEvent.click(screen.getByRole("button", { name: /remove watched_apps item 1/i }));
    expect(store.buildPatches()).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "sampling", "watched_apps"],
    });
  });

  it("renders watched_apps with an empty input when the fetched config omits the field", () => {
    // The TS editor reflects only what the server sent — when the
    // field is absent, the seed is invisible to the operator (this is
    // the documented UX: operators see the seed through the help text,
    // not the input field). Adding an entry promotes the field to an
    // explicit list; the seed is then overridden (per the serde
    // default-fn contract — explicit values beat defaults).
    renderTv({
      sampling: {
        expected_source: "HDMI4",
        source_poll_interval: "15s",
        stream_mode: "warm",
      },
    });
    const items = screen.queryAllByRole("textbox", { name: /watched_apps item/i });
    expect(items).toHaveLength(0);
    // Help text surfaces the seeded-default behavior + opt-out signal.
    expect(screen.getByText(/opt-out/i)).toBeInTheDocument();
    expect(screen.getByText(/daemon-shipped seed/i)).toBeInTheDocument();
  });

  // Clearing a touched text field must emit a remove patch, NOT set "".
  // The server rejects empty strings (validate.rs:1554-1599 + humantime
  // parse), so the only way to express "unset" on a touch-cleared field
  // is to remove the key. This applies to all three text inputs and the
  // stream_mode select (the select already does this via the empty-value
  // sentinel; tests 5 + the new pinning below lock all four together).
  it("clearing compositor_output emits remove (not set \"\")", () => {
    const store = renderTv({ compositor_output: "HDMI-A-1" });
    fireEvent.change(screen.getByLabelText("compositor_output"), { target: { value: "" } });
    const patches = store.buildPatches();
    expect(patches).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "compositor_output"],
    });
    // The legacy `set ""` form must never appear — the server rejects it.
    expect(patches).not.toContainEqual(expect.objectContaining({
      op: "set",
      path: ["displays", "tv", "compositor_output"],
      value: "",
    }));
  });

  it("clearing expected_source emits remove (not set \"\")", () => {
    const store = renderTv({ sampling: { expected_source: "HDMI4", source_poll_interval: "15s" } });
    fireEvent.change(screen.getByLabelText("expected_source"), { target: { value: "" } });
    const patches = store.buildPatches();
    expect(patches).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "sampling", "expected_source"],
    });
    expect(patches).not.toContainEqual(expect.objectContaining({
      op: "set",
      path: ["displays", "tv", "sampling", "expected_source"],
      value: "",
    }));
  });

  it("clearing source_poll_interval emits remove (not set \"\")", () => {
    const store = renderTv({ sampling: { source_poll_interval: "30s" } });
    fireEvent.change(screen.getByLabelText("source_poll_interval"), { target: { value: "" } });
    const patches = store.buildPatches();
    expect(patches).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "sampling", "source_poll_interval"],
    });
    expect(patches).not.toContainEqual(expect.objectContaining({
      op: "set",
      path: ["displays", "tv", "sampling", "source_poll_interval"],
      value: "",
    }));
  });

  it("whitespace-only input on a text field also emits remove (server rejects whitespace)", () => {
    const store = renderTv({ compositor_output: "HDMI-A-1" });
    fireEvent.change(screen.getByLabelText("compositor_output"), { target: { value: "   " } });
    const patches = store.buildPatches();
    expect(patches).toContainEqual({
      op: "remove",
      path: ["displays", "tv", "compositor_output"],
    });
  });

  // SHOULD fix: after typing then clearing, the field must show the
  // cleared state, not snap back to the fetched value. The pending
  // remove is invisible to the editor's `effective()` helper today
  // (getEdit returns undefined for removals), so when the parent
  // re-renders (via the dirtyVersion counter on onDirty in
  // SettingsForm.tsx), the React control snaps back to the stale
  // fetched value while a remove patch stays queued.
  //
  // To verify the post-clear display state we wire `onDirty` to an
  // RTL `rerender` trigger — mirroring SettingsForm's dirtyVersion
  // counter — so the editor re-renders with the pending remove.
  it("after edit-then-clear, the input shows the cleared state (not the fetched value)", () => {
    const cfg: DisplayConfig = { ...TV_DISPLAY, compositor_output: "HDMI-A-1" };
    const store = createPatchStore();
    const onDirty = () => { rerender(<DisplaysSection
      displays={{ tv: cfg }}
      store={store}
      redactedPaths={[]}
      onDirty={onDirty}
      fieldErrors={{}}
      entityCrudEnabled={false}
      rules={{}}
    />); };
    const { rerender } = render(
      <DisplaysSection
        displays={{ tv: cfg }}
        store={store}
        redactedPaths={[]}
        onDirty={onDirty}
        fieldErrors={{}}
        entityCrudEnabled={false}
        rules={{}}
      />,
    );
    const input = screen.getByLabelText("compositor_output") as HTMLInputElement;
    expect(input.value).toBe("HDMI-A-1");
    fireEvent.change(input, { target: { value: "HDMI-A-2" } });
    expect(input.value).toBe("HDMI-A-2");
    fireEvent.change(input, { target: { value: "" } });
    // After clear, the re-render driven by onDirty must surface the
    // cleared/inherit state, NOT the previously-fetched "HDMI-A-1".
    // (Without the Should fix, the editor falls back to the fetched
    // prop because getEdit returns undefined for removals.)
    expect(input.value).toBe("");
  });
});
