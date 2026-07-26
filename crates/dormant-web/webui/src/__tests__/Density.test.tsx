/**
 * W1-5 density tests — localStorage collapse persistence,
 * entity storage keys, and changed-field marker rendering.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import {
  entityStorageKey,
  readEntityExpanded,
  writeEntityExpanded,
  readSectionAdvanced,
  writeSectionAdvanced,
} from "../app/config/density";
import WearSection from "../app/config/WearSection";
import { createPatchStore } from "../app/config/patch";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("density helpers", () => {
  it("entityStorageKey produces stable scoped keys", () => {
    expect(entityStorageKey("sensors", "desk-mmwave")).toBe(
      "dormant-config-collapse:sensors:desk-mmwave",
    );
    expect(entityStorageKey("rules", "office-rule")).toBe(
      "dormant-config-collapse:rules:office-rule",
    );
  });

  it("readEntityExpanded defaults to true when key absent", () => {
    expect(readEntityExpanded("sensors", "nonexistent")).toBe(true);
  });

  it("readEntityExpanded returns false when stored as '0'", () => {
    localStorage.setItem("dormant-config-collapse:sensors:test", "0");
    expect(readEntityExpanded("sensors", "test")).toBe(false);
  });

  it("readEntityExpanded returns true when stored as '1'", () => {
    localStorage.setItem("dormant-config-collapse:sensors:test", "1");
    expect(readEntityExpanded("sensors", "test")).toBe(true);
  });

  it("writeEntityExpanded round-trips", () => {
    writeEntityExpanded("sensors", "test", false);
    expect(readEntityExpanded("sensors", "test")).toBe(false);
    writeEntityExpanded("sensors", "test", true);
    expect(readEntityExpanded("sensors", "test")).toBe(true);
  });

  it("readSectionAdvanced defaults to false", () => {
    expect(readSectionAdvanced("daemon")).toBe(false);
  });

  it("writeSectionAdvanced round-trips", () => {
    writeSectionAdvanced("daemon", true);
    expect(readSectionAdvanced("daemon")).toBe(true);
    writeSectionAdvanced("daemon", false);
    expect(readSectionAdvanced("daemon")).toBe(false);
  });
});

describe("changed-field marker via WearSection", () => {
  it("displays 'changed · was {old}' when a wear field is edited", () => {
    const store = createPatchStore();

    const { rerender } = render(
      <WearSection
        wear={{ enabled: true, sample_interval: "60s" }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    // Edit sample_interval
    const input = screen.getByLabelText("sample_interval") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "120s" } });

    // Force re-render so the changed marker appears
    rerender(
      <WearSection
        wear={{ enabled: true, sample_interval: "60s" }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    expect(screen.getByText(/changed · was 60s/)).toBeInTheDocument();
  });
});
