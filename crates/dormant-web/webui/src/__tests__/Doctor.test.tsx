import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Doctor from "../app/views/Doctor";


const { mocks } = vi.hoisted(() => {
  const runDoctor = vi.fn().mockResolvedValue({
    checks: [
      { name: "Config valid", status: "ok" as const, detail: "config.toml parsed without errors" },
      { name: "IPC socket reachable", status: "ok" as const, detail: "/run/dormant.sock responds" },
      { name: "MQTT broker connection", status: "ok" as const },
      { name: "Sensor stale check", status: "skip" as const, detail: "no sensors are currently stale" },
      { name: "KWin DPMS controller", status: "fail" as const, detail: "DBus service not reachable" },
      { name: "DDC/CI device present", status: "not_supported" as const, detail: "no DDC/CI displays detected" },
    ],
  });
  return {
    mocks: { runDoctor },
  };
});

vi.mock("../api/client", () => ({
  runDoctor: mocks.runDoctor,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Doctor", () => {
  it("renders Run button and empty state before first run", () => {
    render(<Doctor />);

    expect(screen.getByText("▶ Run doctor")).toBeInTheDocument();
    expect(screen.getByText(/Run diagnostics/)).toBeInTheDocument();
  });

  it("runs doctor on button click and renders results", async () => {
    render(<Doctor />);

    fireEvent.click(screen.getByText("▶ Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(mocks.runDoctor).toHaveBeenCalledTimes(1);
  });

  it("renders summary cards with correct counts", async () => {
    render(<Doctor />);

    fireEvent.click(screen.getByText("▶ Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.getByText("Skipped")).toBeInTheDocument();
    expect(screen.getByText("Failing")).toBeInTheDocument();

    const threeVals = screen.getAllByText("3");
    expect(threeVals.length).toBeGreaterThanOrEqual(1);
    const twoVals = screen.getAllByText("2");
    expect(twoVals.length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("1")).toBeInTheDocument();
  });

  it("renders check detail lines and status tags", async () => {
    render(<Doctor />);

    fireEvent.click(screen.getByText("▶ Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(screen.getByText("config.toml parsed without errors")).toBeInTheDocument();
    expect(screen.getByText("/run/dormant.sock responds")).toBeInTheDocument();
    expect(screen.getByText("DBus service not reachable")).toBeInTheDocument();

    expect(screen.getAllByText("ok").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("skip")).toBeInTheDocument();
    expect(screen.getByText("fail")).toBeInTheDocument();
    expect(screen.getByText("n/a")).toBeInTheDocument();
  });

  it("shows loading state while running", () => {
    vi.mocked(mocks.runDoctor).mockReturnValueOnce(new Promise(() => {}));

    render(<Doctor />);
    fireEvent.click(screen.getByText("▶ Run doctor"));

    expect(screen.getByText("Running…")).toBeInTheDocument();
  });

  it("changes button text after first run", async () => {
    render(<Doctor />);

    fireEvent.click(screen.getByText("▶ Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("⟳ Run again")).toBeInTheDocument();
    });
  });
});
