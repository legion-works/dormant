/**
 * KeymapSection tests — round-trip claim_hotkey, <kbd> preview,
 * and store integration.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import KeymapSection from "../app/config/KeymapSection";
import { createPatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("KeymapSection", () => {
  it("renders claim_hotkey text field", () => {
    const store = createPatchStore();

    render(
      <KeymapSection keymap={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    expect(screen.getByLabelText("claim_hotkey")).toBeInTheDocument();
  });

  it("renders <kbd> preview from existing value", () => {
    const store = createPatchStore();

    render(
      <KeymapSection keymap={{ claim_hotkey: "kVK_F3" }} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    expect(screen.getByText("F3")).toBeInTheDocument();
  });

  it("renders — when no hotkey is set", () => {
    const store = createPatchStore();

    render(
      <KeymapSection keymap={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    expect(screen.getByText("—")).toBeInTheDocument();
  });

  it("editing claim_hotkey creates a patch", () => {
    const store = createPatchStore();

    render(
      <KeymapSection keymap={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    const input = screen.getByLabelText("claim_hotkey") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "kVK_F9" } });

    const patches = store.buildPatches();
    const p = patches.find((x) => "path" in x && x.path.join(".") === "keymap.claim_hotkey");
    expect(p).toBeDefined();
    expect((p as { value: unknown }).value).toBe("kVK_F9");
  });
});
