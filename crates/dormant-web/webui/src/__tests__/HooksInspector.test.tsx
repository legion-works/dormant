/**
 * HooksInspector tests — read-only render of five hook slots,
 * abort-gate tags, empty slots, and the footer disclaimer.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import HooksInspector from "../app/config/HooksInspector";
import type { HookSlots } from "../api/types";
import type { PatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const FULL_HOOKS: HookSlots = {
  before_release: [
    { command: ["echo", "releasing"], timeout: "5s", blocking: true, abort_on_failure: true },
    { mqtt: { topic: "dormant/status", payload: "releasing" } },
  ],
  after_release: [],
  before_acquire: [
    { command: ["echo", "acquiring"], timeout: "3s" },
  ],
  after_acquire: [],
  on_observed_loss: [
    { command: ["notify-send", "panel lost"] },
  ],
};

describe("HooksInspector", () => {
  it("renders all five slot labels", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    expect(screen.getByText("before_release")).toBeInTheDocument();
    expect(screen.getByText("after_release")).toBeInTheDocument();
    expect(screen.getByText("before_acquire")).toBeInTheDocument();
    expect(screen.getByText("after_acquire")).toBeInTheDocument();
    expect(screen.getByText("on_observed_loss")).toBeInTheDocument();
  });

  it("renders command argv for filled slots", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    expect(screen.getByText(/echo releasing/)).toBeInTheDocument();
    expect(screen.getByText(/echo acquiring/)).toBeInTheDocument();
    expect(screen.getByText(/notify-send panel lost/)).toBeInTheDocument();
  });

  it("renders MQTT actions", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    expect(screen.getByText(/MQTT dormant\/status ← "releasing"/)).toBeInTheDocument();
  });

  it("renders — none for empty slots", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    // after_release and after_acquire are empty
    const noneEls = screen.getAllByText("— none");
    expect(noneEls.length).toBeGreaterThanOrEqual(2);
  });

  it("renders abort-gate tag on blocking before_* actions", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    // before_release and before_acquire should have abort-gate tags
    const tags = screen.getAllByText("abort-gate");
    expect(tags.length).toBeGreaterThanOrEqual(2);
  });

  it("does not render abort-gate on non-blocking or after_* actions", () => {
    render(<HooksInspector
      hooks={{
        after_release: [{ command: ["echo"], blocking: false }],
      }}
      displayId="tv"
    />);

    expect(screen.queryByText("abort-gate")).toBeNull();
  });

  it("renders footer disclaimer about config-file editing", () => {
    render(<HooksInspector hooks={{}} displayId="tv" />);

    expect(screen.getByText(/Hooks are edited/)).toBeInTheDocument();
  });

  it("renders timeout and other action metadata", () => {
    render(<HooksInspector hooks={FULL_HOOKS} displayId="tv" />);

    expect(screen.getByText(/timeout: 5s/)).toBeInTheDocument();
    expect(screen.getByText(/timeout: 3s/)).toBeInTheDocument();
    expect(screen.getByText(/abort on failure/)).toBeInTheDocument();
  });

  // ── Edit-mode tests (BG-6 / S1) ────────────────────────────────────────

  /** Minimal mock PatchStore that records edits. */
  function mockStore(): PatchStore {
    const edits: Record<string, unknown> = {};
    return {
      trackEdit(path, value) { edits[path.join("\x1E")] = value; },
      trackRemove() {},
      getEdit() { return undefined; },
      trackCreate() {},
      trackDelete() {},
      buildPatches() { return []; },
      isLocked() { return false; },
      reset() {},
    };
  }

  it("shows + Add action button on empty slots when hookEditEnabled is true", () => {
    render(
      <HooksInspector
        hooks={{}}
        displayId="tv"
        hookEditEnabled
        store={mockStore()}
        onDirty={() => {}}
      />,
    );

    // Five empty slots, each with an "+ Add action" button.
    const addButtons = screen.getAllByText(/\+ Add action/);
    expect(addButtons.length).toBe(5);
    // Empty slots should NOT show "— none" when editing.
    expect(screen.queryByText("— none")).not.toBeInTheDocument();
  });

  it("renders MQTT topic and payload fields in edit mode", () => {
    render(
      <HooksInspector
        hooks={{
          before_release: [{ mqtt: { topic: "dormant/status", payload: "test" } }],
        }}
        displayId="tv"
        hookEditEnabled
        store={mockStore()}
        onDirty={() => {}}
      />,
    );

    // The MQTT topic and payload TextFields should render.
    expect(screen.getByDisplayValue("dormant/status")).toBeInTheDocument();
    expect(screen.getByDisplayValue("test")).toBeInTheDocument();
  });

  it("renders footer with editor notice when hookEditEnabled is true", () => {
    render(
      <HooksInspector
        hooks={{}}
        displayId="tv"
        hookEditEnabled
        store={mockStore()}
        onDirty={() => {}}
      />,
    );

    expect(screen.getByText(/Hook commands run with the daemon/)).toBeInTheDocument();
  });
});
