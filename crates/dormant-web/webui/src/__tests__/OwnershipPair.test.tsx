/**
 * OwnershipPair tests — three sizes, agreement verdict arms,
 * hex rendering, panel state.
 *
 * Acceptance criteria from views/switching.md §3:
 * - renders observed_input_code in hex
 * - agreement verdict distinguishes ours / peer's / third input / unreadable
 * - both machines' four codes
 * - panel state with poll cadence
 * - one implementation, three sizes
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
    // Both the observed line and machine code lines show hex — use specific targets.
    expect(screen.getByText(/input 0x0f \(15\)/)).toBeInTheDocument();
  });

  it("shows ours verdict when observed matches local read code", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x0f })} config={dc({ shared_input_code: 0x0f })} size="full" />);
    expect(screen.getByText(/✓ agrees.*ours/)).toBeInTheDocument();
  });

  it("shows peer's-input verdict when observed matches peer read code", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x10 })} config={dc({ shared_input_code: 0x0f, shared_peer_input_code: 0x10 })} size="full" />);
    expect(screen.getByText("peer's input")).toBeInTheDocument();
  });

  it("shows third-input verdict when observed matches neither", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: 0x99 })} config={dc()} size="full" />);
    expect(screen.getByText(/third input/)).toBeInTheDocument();
  });

  it("shows unreadable when observed_input_code is null", () => {
    render(<OwnershipPair displayId="test" snap={snap({ observed_input_code: null })} config={dc()} size="full" />);
    expect(screen.getByText("unreadable")).toBeInTheDocument();
  });

  it("renders both machines' four codes at full size", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc()} size="full" />);
    // Use specific code patterns to avoid ambiguity.
    expect(screen.getByText(/read 0x0f/)).toBeInTheDocument();
    expect(screen.getByText(/write 0x15/)).toBeInTheDocument();
    expect(screen.getByText(/read 0x10/)).toBeInTheDocument();
    expect(screen.getByText(/write 0x11/)).toBeInTheDocument();
  });

  it("renders same-as-read hint when write code equals read code", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc({ shared_input_code: 0x0f, shared_input_write_code: undefined })} size="full" />);
    expect(screen.getByText(/same as read/)).toBeInTheDocument();
  });

  it("renders panel state at full size", () => {
    render(<OwnershipPair displayId="test" snap={snap({ panel_state: { power: "standby", brightness: undefined } })} config={dc()} size="full" />);
    expect(screen.getByText(/STANDBY/)).toBeInTheDocument();
  });

  it("renders poll cadence text at full size", () => {
    render(<OwnershipPair displayId="test" snap={snap()} config={dc()} coordination={{ poll_interval: "3s", loss_confirmations: 5 }} size="full" />);
    expect(screen.getByText(/ownership every 3s/)).toBeInTheDocument();
    expect(screen.getByText(/5 reads to flip/)).toBeInTheDocument();
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
