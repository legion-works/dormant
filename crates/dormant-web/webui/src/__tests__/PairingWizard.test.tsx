/**
 * PairingWizard — the Samsung pairing wizard component (spec §8, T6).
 *
 * Host input -> POST /api/pair/samsung -> 202 {pair_id} -> poll GET
 * every ~1s (overridden to a few ms here for test speed) rendering
 * pairing ("accept on TV" copy) / paired / timeout / error states.
 * Feature-off (pairing_enabled=false) hides the whole component.
 * After paired: a "create display?" hand-off pre-filling host +
 * controllers=["samsung-tizen"].
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, cleanup, waitFor } from "@testing-library/react";
import PairingWizard from "../app/config/PairingWizard";
import { postPairSamsung, getPairStatus, ApiError } from "../api/client";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    postPairSamsung: vi.fn(),
    getPairStatus: vi.fn(),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const mockedPost = vi.mocked(postPairSamsung);
const mockedGet = vi.mocked(getPairStatus);

describe("PairingWizard — feature gating", () => {
  it("renders nothing when pairingEnabled is false", () => {
    const { container } = render(<PairingWizard pairingEnabled={false} pollIntervalMs={5} />);
    expect(container.firstChild).toBeNull();
    expect(mockedPost).not.toHaveBeenCalled();
  });

  it("renders the host input when pairingEnabled is true", () => {
    render(<PairingWizard pairingEnabled={true} pollIntervalMs={5} />);
    expect(screen.getByLabelText(/host/i)).toBeInTheDocument();
  });
});

describe("PairingWizard — poll loop: pairing -> paired", () => {
  it("POSTs the host, then polls until paired, rendering the accept-on-TV copy while pairing", async () => {
    mockedPost.mockResolvedValueOnce({ pair_id: "pid-1" });
    mockedGet
      .mockResolvedValueOnce({ state: "pairing" })
      .mockResolvedValueOnce({ state: "pairing" })
      .mockResolvedValueOnce({ state: "paired" });

    render(<PairingWizard pairingEnabled={true} pollIntervalMs={5} />);

    fireEvent.change(screen.getByLabelText(/host/i), { target: { value: "192.0.2.1" } });
    fireEvent.click(screen.getByRole("button", { name: /^pair$/i }));

    await waitFor(() => expect(mockedPost).toHaveBeenCalledWith("192.0.2.1"));

    await waitFor(() => {
      expect(screen.getByText(/accept.*allow.*prompt/i)).toBeInTheDocument();
    });

    await waitFor(
      () => {
        expect(screen.getByText(/paired/i)).toBeInTheDocument();
      },
      { timeout: 2000 },
    );

    expect(mockedGet).toHaveBeenCalledWith("pid-1");
  });

  it("offers a 'create display?' hand-off after paired, pre-filling host + controllers=[samsung-tizen]", async () => {
    mockedPost.mockResolvedValueOnce({ pair_id: "pid-2" });
    mockedGet.mockResolvedValueOnce({ state: "paired" });

    let handoff: { host: string; controllers: string[] } | null = null;
    render(
      <PairingWizard
        pairingEnabled={true}
        pollIntervalMs={5}
        onDisplayCreateRequest={(p) => { handoff = p; }}
      />,
    );

    fireEvent.change(screen.getByLabelText(/host/i), { target: { value: "192.0.2.9" } });
    fireEvent.click(screen.getByRole("button", { name: /^pair$/i }));

    const createBtn = await screen.findByRole("button", { name: /create.*display/i });
    fireEvent.click(createBtn);

    expect(handoff).toEqual({ host: "192.0.2.9", controllers: ["samsung-tizen"] });
  });
});

describe("PairingWizard — timeout state", () => {
  it("renders a timeout message and a retry affordance, without polling further", async () => {
    mockedPost.mockResolvedValueOnce({ pair_id: "pid-3" });
    mockedGet.mockResolvedValueOnce({ state: "timeout", detail: "no response from TV within 60s" });

    render(<PairingWizard pairingEnabled={true} pollIntervalMs={5} />);
    fireEvent.change(screen.getByLabelText(/host/i), { target: { value: "192.0.2.2" } });
    fireEvent.click(screen.getByRole("button", { name: /^pair$/i }));

    await waitFor(() => {
      expect(screen.getByText(/timed out|timeout/i)).toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: /try again/i })).toBeInTheDocument();
  });
});

describe("PairingWizard — error handling", () => {
  it("shows an inline error when the initial POST is rejected (409 pairing_in_progress)", async () => {
    mockedPost.mockRejectedValueOnce(new ApiError(409, { error: "pairing_in_progress" }));

    render(<PairingWizard pairingEnabled={true} pollIntervalMs={5} />);
    fireEvent.change(screen.getByLabelText(/host/i), { target: { value: "192.0.2.3" } });
    fireEvent.click(screen.getByRole("button", { name: /^pair$/i }));

    await waitFor(() => {
      expect(screen.getByText(/pairing_in_progress/i)).toBeInTheDocument();
    });
    expect(mockedGet).not.toHaveBeenCalled();
  });
});
