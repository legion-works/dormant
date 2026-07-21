import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import DormantPairing from "../app/config/DormantPairing";
import {
  getInstancePairPeers,
  getInstancePairStatus,
  postInstancePair,
  postJoinInstancePair,
} from "../api/client";

vi.mock("../api/client", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api/client")>()),
  getInstancePairPeers: vi.fn(),
  getInstancePairStatus: vi.fn(),
  postInstancePair: vi.fn(),
  postJoinInstancePair: vi.fn(),
}));

afterEach(() => { cleanup(); vi.clearAllMocks(); });

const peers = {
  discovered: [{ instance_id: "public-instance-id", display_name: "Office Mac", pairing_port: 4242, window_id: "public-window" }],
  paired: [{ instance_id: "paired-id", display_name: "Living Room", paired_at: "2026-07-21T00:00:00Z" }],
};

describe("DormantPairing", () => {
  it("shows discovered and paired peers", async () => {
    vi.mocked(getInstancePairPeers).mockResolvedValue(peers);
    render(<DormantPairing enabled />);
    expect(await screen.findByText(/Office Mac/)).toBeInTheDocument();
    expect(screen.getByText(/Living Room/)).toBeInTheDocument();
  });

  it("requires explicit code confirmation", async () => {
    vi.mocked(getInstancePairPeers).mockResolvedValue(peers);
    vi.mocked(postJoinInstancePair).mockResolvedValue({ state: "pairing" });
    render(<DormantPairing enabled />);
    await screen.findByText(/Office Mac/);
    expect(screen.getByRole("button", { name: /confirm and join/i })).toBeDisabled();
    fireEvent.click(screen.getByLabelText(/Office Mac/));
    fireEvent.change(screen.getByLabelText(/pairing code/i), { target: { value: "ABCD1234" } });
    fireEvent.click(screen.getByRole("button", { name: /confirm and join/i }));
    await waitFor(() => expect(postJoinInstancePair).toHaveBeenCalledWith("Office Mac", "public-instance-id", "ABCD1234"));
  });

  it("renders expiry and retry", async () => {
    vi.mocked(getInstancePairPeers).mockResolvedValue({ discovered: [], paired: [] });
    vi.mocked(postInstancePair).mockResolvedValue({ pair_id: "p1", code: "ABCD1234", expires_at: "2026-07-21T01:00:00Z" });
    render(<DormantPairing enabled />);
    fireEvent.change(screen.getByLabelText(/local display name/i), { target: { value: "Desk" } });
    fireEvent.click(screen.getByRole("button", { name: /open pairing window/i }));
    expect(await screen.findByText(/Expires: 2026-07-21T01:00:00Z/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /retry discovery/i }));
    expect(getInstancePairPeers).toHaveBeenCalledTimes(2);
  });

  it("never renders private key material", async () => {
    const privateKey = "SENTINEL-PRIVATE-KEY";
    vi.mocked(getInstancePairPeers).mockResolvedValue(peers);
    vi.mocked(getInstancePairStatus).mockResolvedValue({ state: "paired", detail: null });
    render(<DormantPairing enabled />);
    await screen.findByText(/Office Mac/);
    expect(screen.queryByText(privateKey)).toBeNull();
    expect(document.body.textContent).not.toContain(privateKey);
  });
});
