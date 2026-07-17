/**
 * SidebarFooter — P1-D daemon identity block (pid/uptime/socket + fleet
 * label), received from the shell's shared `GET /api/daemon` request.
 * Verifies the render contract only — the endpoint itself is exercised by
 * dormant-web's own `build_router_daemon_*` tests.
 */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import SidebarFooter from "../app/components/SidebarFooter";

const DAEMON = {
  pid: 48213,
  started_epoch_s: Math.floor(Date.now() / 1000) - (6 * 3600 + 12 * 60),
  version: "0.2.0",
  socket: "/run/dormant/dormant.sock",
};

afterEach(() => {
  cleanup();
});

describe("SidebarFooter", () => {
  it("renders pid/uptime and the socket path from the shell identity", () => {
    render(<SidebarFooter connected daemon={DAEMON} />);

    expect(screen.getByText(/pid 48213/)).toBeInTheDocument();
    expect(screen.getByText(/up 6h 12m/)).toBeInTheDocument();
    expect(screen.getByText("/run/dormant/dormant.sock")).toBeInTheDocument();
    expect(screen.getByText("Legion fleet daemon")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "GitHub repository" })).toHaveAttribute(
      "href",
      "https://github.com/legion-works/dormant",
    );
  });

  it("shows the connection label without the daemon block before identity resolves", () => {
    render(<SidebarFooter connected={false} daemon={null} />);

    expect(screen.getByText("connecting…")).toBeInTheDocument();
    expect(screen.queryByText(/pid /)).not.toBeInTheDocument();
  });

  it("renders a known daemon identity while the event stream reconnects", () => {
    render(<SidebarFooter connected={false} daemon={DAEMON} />);

    expect(screen.getByText("connecting…")).toBeInTheDocument();
    expect(screen.getByText(/pid 48213/)).toBeInTheDocument();
  });
});
