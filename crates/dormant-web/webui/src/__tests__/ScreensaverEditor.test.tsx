/**
 * ScreensaverEditor component tests — source add/remove, ancestor-lock.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import ScreensaverEditor from "../app/config/ScreensaverEditor";
import type { ScreensaverConfig, ScreensaverSource, ConfigPatch } from "../api/types";

const FIXTURE_SS: ScreensaverConfig = {
  trigger: "escalation",
  audio: false,
  scale_mode: "fill",
  transition: "crossfade",
  transition_duration: "1s",
  source: [
    { path: "/usr/share/wallpapers", recurse: true, shuffle: false, order: "sequential", image_duration: "10s" },
  ],
};

function renderEditor(screensaver: ScreensaverConfig = FIXTURE_SS, redactedPaths: string[][] = []) {
  const store = createPatchStore();
  const onDirty = () => {};
  const result = render(
    <ScreensaverEditor
      screensaver={screensaver}
      displayId="tv"
      store={store}
      redactedPaths={redactedPaths}
      onDirty={onDirty}
      fieldErrors={{}}
    />,
  );
  return { ...result, store };
}

describe("ScreensaverEditor", () => {
  afterEach(() => {
    cleanup();
  });

  it("add source emits whole-array set on screensaver.source", async () => {
    const { store } = renderEditor();

    const addBtn = screen.getByRole("button", { name: /add source/i });
    fireEvent.click(addBtn);

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");
    expect(patches[0].path).toEqual(["displays", "tv", "screensaver", "source"]);

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as ScreensaverSource[];
    expect(value).toHaveLength(2);
    expect(value[0]).toEqual(FIXTURE_SS.source[0]);
    // New source has default fields
    expect(value[1]).toMatchObject({ recurse: false, shuffle: false });
  });

  it("remove source emits whole-array set on screensaver.source", async () => {
    const ss: ScreensaverConfig = {
      ...FIXTURE_SS,
      source: [
        { path: "/a" },
        { path: "/b" },
      ],
    };
    const { store } = renderEditor(ss);

    const removeBtns = screen.getAllByRole("button", { name: /remove source/i });
    expect(removeBtns).toHaveLength(2);
    fireEvent.click(removeBtns[1]); // Remove "/b"

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");
    expect(patches[0].path).toEqual(["displays", "tv", "screensaver", "source"]);

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as ScreensaverSource[];
    expect(value).toHaveLength(1);
    expect(value[0]).toEqual({ path: "/a" });
  });

  it("locks sources editor when redacted path is a descendant of a source path", async () => {
    // redacted_paths: [[displays,tv,screensaver,source,0,urls,0]]
    // This is a descendant of screensaver.source — the ENTIRE sources editor locks
    const redacted: string[][] = [
      ["displays", "tv", "screensaver", "source", "0", "urls", "0"],
    ];

    renderEditor(FIXTURE_SS, redacted);

    // Sources area should show a lock indicator
    const lockEls = screen.getAllByTitle(/contains credentialed URLs/);
    expect(lockEls.length).toBeGreaterThanOrEqual(1);
  });

  it("renders scalar fields (audio, scale_mode, transition, transition_duration)", async () => {
    renderEditor();

    // These labels should be present
    expect(screen.getByLabelText("audio")).toBeInTheDocument();
    expect(screen.getByLabelText("scale_mode")).toBeInTheDocument();
    expect(screen.getByLabelText("transition")).toBeInTheDocument();
    expect(screen.getByLabelText("transition_duration")).toBeInTheDocument();
  });
});
