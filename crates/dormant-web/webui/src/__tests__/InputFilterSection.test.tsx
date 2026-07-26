/**
 * InputFilterSection tests — round-trip ignore_devices string list,
 * doctor-probe link, and Linux input-group help text.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import InputFilterSection from "../app/config/InputFilterSection";
import { createPatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("InputFilterSection", () => {
  it("renders the string-list editor for ignore_devices", () => {
    const store = createPatchStore();

    render(
      <InputFilterSection inputFilter={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    expect(screen.getByLabelText("ignore_devices")).toBeInTheDocument();
  });

  it("renders existing devices as chips", () => {
    const store = createPatchStore();

    render(
      <InputFilterSection
        inputFilter={{ ignore_devices: ["/dev/input/event3", "/dev/input/event5"] }}
        store={store}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    // StringListField renders device paths as editable input values, not text nodes.
    expect(screen.getByDisplayValue("/dev/input/event3")).toBeInTheDocument();
    expect(screen.getByDisplayValue("/dev/input/event5")).toBeInTheDocument();
  });

  it("links to the input-filter doctor probe", () => {
    const store = createPatchStore();

    render(
      <InputFilterSection inputFilter={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    const link = screen.getByText(/run doctor input-filter/);
    expect(link.tagName).toBe("A");
    expect(link.getAttribute("href")).toBe("#/doctor?subject=input-filter");
  });

  it("states the Linux input-group requirement", () => {
    const store = createPatchStore();

    render(
      <InputFilterSection inputFilter={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    expect(screen.getByText(/input.*group/)).toBeInTheDocument();
  });

  it("adding an entry creates a patch", () => {
    const store = createPatchStore();

    render(
      <InputFilterSection inputFilter={{}} store={store} onDirty={() => {}} fieldErrors={{}} />,
    );

    const addInput = screen.getByLabelText("ignore_devices") as HTMLInputElement;
    fireEvent.change(addInput, { target: { value: "/dev/input/event4" } });
    fireEvent.click(screen.getByRole("button", { name: /add ignore_devices/i }));

    const patches = store.buildPatches();
    const p = patches.find((x) => "path" in x && x.path.join(".") === "input_filter.ignore_devices");
    expect(p).toBeDefined();
    // The value should be an array containing the entry
    const val = (p as { value: unknown }).value;
    expect(Array.isArray(val)).toBe(true);
    expect(val).toContain("/dev/input/event4");
  });
});
