/**
 * OwnershipPair tests — three sizes, agreement verdict arms,
 * hex rendering, panel state.
 *
 * v3 fidelity restructure: full size now uses a three-region grid
 * (this-machine | panel-state box | peer) matching screens/02.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import OwnershipPair from "../app/views/OwnershipPair";
import type { DisplaySnapshot, DisplayConfig } from "../api/types";

afterEach(() => cleanup());

function snap(overrides: Partial<DisplaySnapshot> = {}): DisplaySnapshot {
  return {
    phase: "active", inhibited: false, paused: false, cmd_gen: 1,
    controllers: [], scope: "shared", owned: true,
    observed_input_code: 0x0f,
    panel_state: { power: "on", brightness: 42 },
    ...overrides,
  };
}

function dc(overrides: Partial<DisplayConfig> = {}): DisplayConfig {
  return {
    controllers: ["ddcci"], scope: "shared",
    shared_input_code: 0x0f, shared_input_write_code: 0x15,
    shared_peer_input_code: 0x10, shared_peer_input_write_code: 0x11,
    ...overrides,
  };
}

describe("OwnershipPair", () => {
  it("renders observed_input_code in hex with decimal echo (full size)", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc()} size="full" />);
    // The observed code appears as "observed 0x0f (15)" in the header.
    expect(screen.getByText(/observed 0x0f \(15\)/)).toBeInTheDocument();
  });

  it("shows ours verdict when observed matches local read code", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x0f })} config={dc({ shared_input_code: 0x0f })} size="full" />);
    // Verdict appears in both the center column and the agreement footer.
    expect(screen.getAllByText(/✓ agrees.*ours/).length).toBeGreaterThanOrEqual(1);
  });

  it("shows peer's-input verdict when observed matches peer read code", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x10 })} config={dc({ shared_input_code: 0x0f, shared_peer_input_code: 0x10 })} size="full" />);
    expect(screen.getAllByText("peer's input").length).toBeGreaterThanOrEqual(1);
  });

  it("shows third-input verdict when observed matches neither", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x99 })} config={dc()} size="full" />);
    expect(screen.getAllByText(/third input/).length).toBeGreaterThanOrEqual(1);
  });

  it("shows unreadable when observed_input_code is null", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: null })} config={dc()} size="full" />);
    expect(screen.getAllByText("unreadable").length).toBeGreaterThanOrEqual(1);
  });

  it("renders both machines' four code values at full size", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc()} size="full" />);
    // The three-region restructure renders hex codes as "0x0f (15)" spans
    // rather than "read 0x0f" composite lines.
    expect(screen.getByText("0x0f (15)")).toBeInTheDocument();
    expect(screen.getByText("0x15 (21)")).toBeInTheDocument();
    expect(screen.getByText("0x10 (16)")).toBeInTheDocument();
    expect(screen.getByText("0x11 (17)")).toBeInTheDocument();
  });

  it("renders OURS badge when owned", () => {
    render(<OwnershipPair displayId="test" snap={snap({ owned: true })} config={dc()} size="full" />);
    expect(screen.getByText("OURS")).toBeInTheDocument();
  });

  it("renders panel state box at full size", () => {
    render(<OwnershipPair displayId="test" snap={snap({ panel_state: { power: "on", brightness: 42 } })} config={dc()} size="full" />);
    // The panel state box shows ● ON and brightness.
    expect(screen.getByText("● ON")).toBeInTheDocument();
    expect(screen.getByText(/brightness 42/)).toBeInTheDocument();
  });

  it("renders OFF state when panel is not on", () => {
    render(<OwnershipPair displayId="test" snap={snap({ panel_state: { power: "standby", brightness: undefined } })} config={dc()} size="full" />);
    expect(screen.getByText("○ OFF")).toBeInTheDocument();
  });

  it("renders marker size with only verdict and input code (no machine codes)", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x0f })} config={dc()} size="marker" />);
    expect(screen.getByText(/✓ agrees.*ours/)).toBeInTheDocument();
    // Machine codes (write/read labels) should not be present in marker.
    expect(screen.queryByText(/read 0x/)).toBeNull();
  });

  it("renders compact size with both machine roles", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc()} size="compact" />);
    expect(screen.getByText("this machine")).toBeInTheDocument();
    expect(screen.getByText("peer")).toBeInTheDocument();
  });
});
