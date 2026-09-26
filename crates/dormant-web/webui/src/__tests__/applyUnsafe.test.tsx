/**
 * SettingsForm — apply-bar unsafe-setting confirmation gate.
 *
 * The fail-safe presence default (`unavailable_policy = "present"` per
 * `crates/dormant-core/src/zone.rs:37-45`) is a deliberate invariant
 * the operator is allowed to flip, but the apply bar must show a
 * confirmation step before sending the request: a screen that blanks
 * while someone is sitting at it is the project-defined worst failure
 * mode.
 *
 * The gate is purely client-side UX — the Rust server is unchanged
 * (the value is accepted normally); the detector lives in
 * `entityCrud.ts` next to the other config-CRUD security mirrors.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, act, waitFor } from "@testing-library/react";
import { SettingsForm } from "../app/config/SettingsForm";
import type { ConfigResponse, ApplyResponse } from "../api/types";
import { postConfigApply } from "../api/client";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    getConfig: vi.fn(),
    postConfigApply: vi.fn(),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const F1 = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";

const BASE_CONFIG: ConfigResponse = {
  path: "/home/user/.config/dormant/config.toml",
  config_version: 1,
  source: "last_applied",
  raw_toml: "[daemon]\n",
  inventory: {
    config_version: 1,
    daemon: {},
    sensors: {
      "desk-mmwave": { type: "usb-ld2410", port: "/dev/ttyUSB0" },
    },
    zones: {
      office: { mode: "any", members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" },
    },
    displays: {
      "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" },
    },
    rules: {},
  },
  validation: { ok: true, warnings: [], errors: [] },
  display_rules: {},
  fingerprint: F1,
  redacted_paths: [],
};

const REPLIED: ApplyResponse = { reload: "reloaded", applied: true };

function findZoneField(zone: HTMLElement, label: string) {
  return zone.querySelector(`#${CSS.escape(`zones.office.${label}`)}`) as HTMLSelectElement | null;
}

describe("SettingsForm — Apply bar unsafe-setting confirmation", () => {
  it("with an unsafe Set patch, clicking Apply shows a confirmation; click Apply anyway to send", async () => {
    vi.mocked(postConfigApply).mockResolvedValueOnce(REPLIED);
    render(<SettingsForm config={BASE_CONFIG} tab="presence" />);

    // Open the office zone card and flip unavailable_policy to "absent".
    const officeCard = screen.getByText("office").closest(".cf-card") as HTMLElement;
    const select = findZoneField(officeCard, "unavailable_policy");
    expect(select).not.toBeNull();
    fireEvent.change(select!, { target: { value: "absent" } });

    // Hit Apply.
    fireEvent.click(screen.getByRole("button", { name: /^apply$/i }));

    // A confirmation dialog appears and the apply request is NOT yet sent.
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent(/unavailable_policy.*absent|absent.*offline|absent.*empty|absent.*screen/i);
    expect(vi.mocked(postConfigApply)).not.toHaveBeenCalled();

    // Confirming sends the request.
    fireEvent.click(screen.getByRole("button", { name: /apply anyway/i }));
    await waitFor(() => expect(vi.mocked(postConfigApply)).toHaveBeenCalled());
  });

  it("with an unsafe patch, Cancel sends NO request", async () => {
    vi.mocked(postConfigApply).mockResolvedValueOnce(REPLIED);
    render(<SettingsForm config={BASE_CONFIG} tab="presence" />);

    const officeCard = screen.getByText("office").closest(".cf-card") as HTMLElement;
    const select = findZoneField(officeCard, "unavailable_policy");
    fireEvent.change(select!, { target: { value: "absent" } });

    fireEvent.click(screen.getByRole("button", { name: /^apply$/i }));
    await screen.findByRole("alertdialog");

    fireEvent.click(screen.getByRole("button", { name: /^cancel$/i }));
    // Flush any microtasks the async confirm continuation runs on.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(vi.mocked(postConfigApply)).not.toHaveBeenCalled();
  });

  it("a safe patch (\"present\") applies with NO confirmation", async () => {
    vi.mocked(postConfigApply).mockResolvedValueOnce(REPLIED);
    render(<SettingsForm config={BASE_CONFIG} tab="presence" />);

    const officeCard = screen.getByText("office").closest(".cf-card") as HTMLElement;
    const select = findZoneField(officeCard, "unavailable_policy");
    fireEvent.change(select!, { target: { value: "present" } });

    fireEvent.click(screen.getByRole("button", { name: /^apply$/i }));
    await waitFor(() => expect(vi.mocked(postConfigApply)).toHaveBeenCalled());
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("an unsafe CreateEntity zone (unavailable_policy = \"absent\") triggers the confirmation too", async () => {
    vi.mocked(postConfigApply).mockResolvedValueOnce(REPLIED);
    render(<SettingsForm config={BASE_CONFIG} tab="presence" />);

    // Open the zones Add form.
    fireEvent.click(screen.getByRole("button", { name: /add zone/i }));
    const form = screen.getByTestId("create-zones-form");

    fireEvent.change(within(form).getByLabelText("id"), { target: { value: "lounge" } });
    // unavailable_policy already defaults to "present" — flip it to "absent".
    fireEvent.change(within(form).getByLabelText("unavailable_policy"), { target: { value: "absent" } });
    fireEvent.click(within(form).getByRole("button", { name: /create/i }));

    fireEvent.click(screen.getByRole("button", { name: /^apply$/i }));
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent(/absent|offline|screen|empty/i);
    expect(vi.mocked(postConfigApply)).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /apply anyway/i }));
    await waitFor(() => expect(vi.mocked(postConfigApply)).toHaveBeenCalled());
  });
});

// Helpers — within import not at top to avoid TS noUnused
import { within } from "@testing-library/react";
