/**
 * SidebarFooter — P1-D daemon identity block (pid/uptime/socket + fleet
 * label), fetched once on mount via `GET /api/daemon` and refreshed on
 * reconnect. Verifies the render contract only — the endpoint itself is
 * exercised by dormant-web's own `build_router_daemon_*` tests.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import SidebarFooter from "../app/components/SidebarFooter";

const mocks = vi.hoisted(() => ({
  getDaemon: vi.fn(),
}));

vi.mock("../api/client", () => ({
  getDaemon: mocks.getDaemon,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("SidebarFooter", () => {
  it("renders pid/uptime and the socket path once /api/daemon resolves", async () => {
    mocks.getDaemon.mockResolvedValue({
      pid: 48213,
      started_epoch_s: Math.floor(Date.now() / 1000) - (6 * 3600 + 12 * 60),
      version: "0.2.0",
      socket: "/run/dormant/dormant.sock",
    });

    render(<SidebarFooter connected />);

    await waitFor(() => {
      expect(screen.getByText(/pid 48213/)).toBeInTheDocument();
    });
    expect(screen.getByText(/up 6h 12m/)).toBeInTheDocument();
    expect(screen.getByText("/run/dormant/dormant.sock")).toBeInTheDocument();
    expect(screen.getByText("Legion fleet daemon")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "GitHub repository" })).toHaveAttribute(
      "href",
      "https://github.com/legion-works/dormant",
    );
  });

  it("shows the connection label without the daemon block before the fetch resolves", () => {
    mocks.getDaemon.mockReturnValue(new Promise(() => undefined));

    render(<SidebarFooter connected={false} />);

    expect(screen.getByText("connecting…")).toBeInTheDocument();
    expect(screen.queryByText(/pid /)).not.toBeInTheDocument();
  });

  it("does not crash and stays silent when /api/daemon fails", async () => {
    mocks.getDaemon.mockRejectedValue(new Error("daemon unreachable"));

    render(<SidebarFooter connected={false} />);

    await waitFor(() => expect(mocks.getDaemon).toHaveBeenCalledOnce());
    expect(screen.queryByText(/pid /)).not.toBeInTheDocument();
  });
});
