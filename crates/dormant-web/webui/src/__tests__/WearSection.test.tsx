import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import WearSection from "../app/config/WearSection";
import type { WearConfig } from "../api/types";

afterEach(() => cleanup());

const wear: WearConfig = {
  enabled: true,
  active_sampling: {
    enabled: true,
    sampled_display: "desk",
    stream_mode: "warm",
    capture_timeout: "2s",
    failure_threshold: 5,
    circuit_reset_after: "5m",
  },
};

const displays = {
  desk: { controllers: ["kwin-dpms"] },
  tv: { controllers: ["samsung-tizen"] },
};

function renderSection() {
  const store = createPatchStore();
  render(
    <WearSection
      wear={wear}
      displays={displays}
      store={store}
      redactedPaths={[]}
      onDirty={() => {}}
      fieldErrors={{}}
    />,
  );
  return store;
}

describe("WearSection active sampling", () => {
  it("renders the active-sampling controls and only render-eligible displays", () => {
    renderSection();

    expect(screen.getByLabelText("active_sampling.enabled")).toBeInTheDocument();
    expect(screen.getByLabelText("active_sampling.sampled_display")).toHaveValue("desk");
    expect(screen.getByRole("option", { name: "desk" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "tv" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("active_sampling.stream_mode")).toHaveValue("warm");
    expect(screen.getByRole("option", { name: "per-tick" })).toBeInTheDocument();
  });

  it("patches each active-sampling selector and validated field at its exact path", () => {
    const store = renderSection();

    fireEvent.click(screen.getByLabelText("active_sampling.enabled"));
    fireEvent.change(screen.getByLabelText("active_sampling.sampled_display"), { target: { value: "desk" } });
    fireEvent.change(screen.getByLabelText("active_sampling.stream_mode"), { target: { value: "per-tick" } });
    fireEvent.change(screen.getByLabelText("active_sampling.capture_timeout"), { target: { value: "3s" } });
    fireEvent.change(screen.getByLabelText("active_sampling.failure_threshold"), { target: { value: "7" } });
    fireEvent.change(screen.getByLabelText("active_sampling.circuit_reset_after"), { target: { value: "30s" } });

    expect(store.buildPatches()).toEqual(expect.arrayContaining([
      { op: "set", path: ["wear", "active_sampling", "enabled"], value: false },
      { op: "set", path: ["wear", "active_sampling", "sampled_display"], value: "desk" },
      { op: "set", path: ["wear", "active_sampling", "stream_mode"], value: "per-tick" },
      { op: "set", path: ["wear", "active_sampling", "capture_timeout"], value: "3s" },
      { op: "set", path: ["wear", "active_sampling", "failure_threshold"], value: 7 },
      { op: "set", path: ["wear", "active_sampling", "circuit_reset_after"], value: "30s" },
    ]));
  });

  it("renders server validation detail for an active-sampling field", () => {
    const store = createPatchStore();
    render(<WearSection wear={wear} displays={displays} store={store} redactedPaths={[]} onDirty={() => {}}
      fieldErrors={{ "wear.active_sampling.capture_timeout": "must be no more than half the sample interval" }} />);
    expect(screen.getByText("must be no more than half the sample interval")).toBeInTheDocument();
  });
});

describe("WearSection active sampling plural form", () => {
  const pluralWear: WearConfig = {
    enabled: true,
    active_sampling: {
      enabled: true,
      sampled_displays: ["desk", "tv"],
      stream_mode: "warm",
      capture_timeout: "2s",
      failure_threshold: 5,
      circuit_reset_after: "5m",
    },
  };

  const displays = {
    desk: { controllers: ["kwin-dpms"] },
    tv: { controllers: ["kwin-dpms"] },
  };

  it("renders the multi-select when sampled_displays is the canonical field", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={pluralWear}
        displays={displays}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    expect(screen.getByLabelText("active_sampling.sampled_displays: desk")).toBeChecked();
    expect(screen.getByLabelText("active_sampling.sampled_displays: tv")).toBeChecked();
    // Legacy singular row stays hidden in the plural form to keep the
    // operator from mixing keys (server rejects both at once).
    expect(screen.queryByLabelText("active_sampling.sampled_display")).not.toBeInTheDocument();
  });

  it("patches a sampled_displays toggle at its exact path", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={pluralWear}
        displays={displays}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    fireEvent.click(screen.getByLabelText("active_sampling.sampled_displays: tv"));
    expect(store.buildPatches()).toEqual(expect.arrayContaining([
      { op: "set", path: ["wear", "active_sampling", "sampled_displays"], value: ["desk"] },
    ]));
  });
});

describe("WearSection sampled-displays selector — compositor_output opt-in", () => {
  // Remote-only TV with no compositor_output — must NOT appear in the
  // sampled-displays selector until the operator declares a
  // compositor_output. The Rust `is_sampling_eligible` predicate widens
  // the legacy `is_render_eligible` with a compositor_output OR branch;
  // the TS mirror must do the same.
  const singularWear: WearConfig = {
    enabled: true,
    active_sampling: {
      enabled: true,
      sampled_display: "desk",
      stream_mode: "warm",
      capture_timeout: "2s",
      failure_threshold: 5,
      circuit_reset_after: "5m",
    },
  };

  it("excludes a remote-only TV from sampled-display options when compositor_output is unset", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={singularWear}
        displays={{
          desk: { controllers: ["kwin-dpms"] },
          tv: { controllers: ["samsung-tizen"] },
        }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    expect(screen.getByLabelText("active_sampling.sampled_display")).toHaveValue("desk");
    expect(screen.queryByRole("option", { name: "tv" })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("active_sampling.sampled_display: tv")).not.toBeInTheDocument();
  });

  it("includes a remote-only TV in sampled-display options once compositor_output is non-empty", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={singularWear}
        displays={{
          desk: { controllers: ["kwin-dpms"] },
          tv: { controllers: ["samsung-tizen"], compositor_output: "HDMI-A-1" },
        }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    expect(screen.getByLabelText("active_sampling.sampled_display")).toHaveValue("desk");
    expect(screen.getByRole("option", { name: "tv" })).toBeInTheDocument();
  });

  // Rust `is_sampling_eligible` widens on `is_some()` (schema.rs:1754-1756),
  // TS on `trim().length > 0` (WearSection.tsx). The Rust side rejects
  // whitespace-only compositor_output configs (validate.rs:1554-1563), so
  // for any validated config the two definitions coincide — but the TS
  // guard is what gates the selector. Pin the TS side explicitly so the
  // asymmetry cannot silently flip the other way.
  it("whitespace-only compositor_output does NOT qualify a remote-only display for sampling", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={singularWear}
        displays={{
          desk: { controllers: ["kwin-dpms"] },
          tv: { controllers: ["samsung-tizen"], compositor_output: "   " },
        }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    expect(screen.getByLabelText("active_sampling.sampled_display")).toHaveValue("desk");
    expect(screen.queryByRole("option", { name: "tv" })).not.toBeInTheDocument();
  });

  // Mixed controllers — at least one local present so has_local is true;
  // the controllers list is not composed solely of remote ones so
  // only_remote is false. The display is render-eligible (and therefore
  // sampling-eligible) on its own, with no compositor_output needed.
  it("mixed controllers (one local + one remote) are sampling-eligible without compositor_output", () => {
    const store = createPatchStore();
    render(
      <WearSection
        wear={singularWear}
        displays={{
          desk: { controllers: ["kwin-dpms"] },
          tv: { controllers: ["samsung-tizen", "ddcci"] },
        }}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );
    expect(screen.getByLabelText("active_sampling.sampled_display")).toHaveValue("desk");
    expect(screen.getByRole("option", { name: "tv" })).toBeInTheDocument();
  });
});
