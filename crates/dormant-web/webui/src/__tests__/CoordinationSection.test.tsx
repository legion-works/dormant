/**
 * CoordinationSection tests — renders six config fields, live
 * activation marker, derived latency chip, and client-side validation.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import CoordinationSection from "../app/config/CoordinationSection";
import { createPatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("CoordinationSection", () => {
  it("renders all six fields with defaults", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{}}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByText("poll_interval")).toBeInTheDocument();
    expect(screen.getByText("loss_confirmations")).toBeInTheDocument();
    expect(screen.getByText("activity_follow")).toBeInTheDocument();
    expect(screen.getByText("arm_after")).toBeInTheDocument();
    expect(screen.getByText("cooldown")).toBeInTheDocument();
    // state_poll_interval is behind Advanced
    expect(screen.queryByText("state_poll_interval")).toBeNull();
  });

  it("shows ● active when kvm is present", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{}}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
        kvm={{ keymap: {}, switch_capable_displays: [], activity_following: false, push_capable_displays: [] }}
      />,
    );

    expect(screen.getByText("● active")).toBeInTheDocument();
  });

  it("shows ○ inert when kvm is null", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{}}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
        kvm={null}
      />,
    );

    expect(screen.getByText("○ inert")).toBeInTheDocument();
  });

  it("shows derived latency chip under loss_confirmations", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ poll_interval: "2s", loss_confirmations: 3 }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    // 2s × 3 = 6s
    expect(screen.getByText(/~6.0s to commit/)).toBeInTheDocument();
  });

  it("shows editing loss_confirmations generates a patch", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ loss_confirmations: 3 }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const input = screen.getByLabelText("loss_confirmations") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "5" } });

    const patches = store.buildPatches();
    const lcPatch = patches.find((p) => "path" in p && p.path.join(".") === "coordination.loss_confirmations");
    expect(lcPatch).toBeDefined();
    expect((lcPatch as { value: unknown }).value).toBe(5);
  });

  it("shows client-side validation for loss_confirmations out of range", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ loss_confirmations: 0 }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByText(/must be 1–10/)).toBeInTheDocument();
  });

  it("expand Advanced shows state_poll_interval", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ state_poll_interval: "45s" }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    // Click Advanced toggle
    fireEvent.click(screen.getByText("Advanced"));

    expect(screen.getByLabelText("state_poll_interval")).toBeInTheDocument();
  });

  it("tracks edits through the store", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{}}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const pollInput = screen.getByLabelText("poll_interval") as HTMLInputElement;
    fireEvent.change(pollInput, { target: { value: "5s" } });

    const patches = store.buildPatches();
    expect(patches.some((p) => "path" in p && p.path.join(".") === "coordination.poll_interval")).toBe(true);
  });

  it("latency chip updates live when poll_interval is edited", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ poll_interval: "2s", loss_confirmations: 3 }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByText(/~6\.0s to commit/)).toBeInTheDocument();

    const pollInput = screen.getByLabelText("poll_interval") as HTMLInputElement;
    fireEvent.change(pollInput, { target: { value: "5s" } });

    expect(screen.getByText(/~15\.0s to commit/)).toBeInTheDocument();
  });

  it("latency chip updates live when loss_confirmations is edited", () => {
    const store = createPatchStore();

    render(
      <CoordinationSection
        coordination={{ poll_interval: "2s", loss_confirmations: 3 }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    const confInput = screen.getByLabelText("loss_confirmations") as HTMLInputElement;
    fireEvent.change(confInput, { target: { value: "5" } });

    expect(screen.getByText(/~10\.0s to commit/)).toBeInTheDocument();
  });
});
