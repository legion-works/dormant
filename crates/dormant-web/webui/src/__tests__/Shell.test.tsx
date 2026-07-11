import { describe, it, expect, vi, afterEach } from "vitest";
import { render, waitFor, cleanup } from "@testing-library/react";
import Shell from "../app/Shell";


const { SAMPLE_STATE, ZERO_DISPLAY_STATE, SAMPLE_CONFIG } = vi.hoisted(() => ({
  SAMPLE_STATE: {
    sensors: [
      { id: "s1", state: "present" as const, last_seen_secs_ago: 3 },
    ],
    zones: [
      { id: "z1", present: true },
    ],
    displays: [
      [
        "d1",
        { phase: "active" as const, inhibited: false, paused: false, cmd_gen: 1, controllers: [] },
      ],
    ],
    pending_reload: null,
  },
  ZERO_DISPLAY_STATE: {
    sensors: [{ id: "s1", state: "present" as const, last_seen_secs_ago: 3 }],
    zones: [{ id: "z1", present: true }],
    displays: [],
    pending_reload: null,
  },
  SAMPLE_CONFIG: {
    path: "/tmp/c.toml",
    config_version: 1,
    source: "last_applied" as const,
    raw_toml: "",
    inventory: {
      config_version: 1,
      daemon: {},
      sensors: {},
      zones: {},
      displays: {},
      rules: {},
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {},
  },
}));

vi.mock("../api/ws", () => ({
  useEvents: vi.fn(() => ({ connected: true, close: vi.fn() })),
}));

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  postReload: vi.fn().mockResolvedValue(undefined),
  getWear: vi.fn().mockResolvedValue({ displays: [] }),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Shell", () => {
  it("renders the sidebar navigation with all five views", () => {
    render(<Shell />);

    expect(document.querySelector(".brand-wordmark")?.textContent).toBe("dormant");
    expect(document.querySelector(".brand-sub")?.textContent).toBe("v0.1.0 · pre-alpha");

    const navEl = document.querySelector(".sidebar-nav")!;
    const navLabels = ["Dashboard", "Displays", "Events", "Config", "Doctor"];
    for (const label of navLabels) {
      const found = Array.from(navEl.querySelectorAll(".nav-label")).some(
        (el) => el.textContent === label,
      );
      expect(found).toBe(true);
    }

    expect(document.querySelector(".topbar-title")?.textContent).toBe("Dashboard");
  });

  it("shows the reload button and clock", () => {
    render(<Shell />);

    const reload = document.querySelector(".topbar-reload");
    expect(reload).toBeInTheDocument();
    expect(reload?.textContent).toContain("Reload");

    const clockEl = document.querySelector(".topbar-clock");
    expect(clockEl).toBeInTheDocument();
    expect(clockEl?.textContent).toMatch(/\d{2}:\d{2}:\d{2}/);
  });

  it("shows the live display count as the Displays nav badge", async () => {
    render(<Shell />);

    // Wait for the LiveStateProvider to resolve the mocked API calls.
    await waitFor(() => {
      const badge = document.querySelector(".nav-badge");
      // SAMPLE_STATE has 1 display — badge should show "1".
      expect(badge).toBeInTheDocument();
      expect(badge?.textContent).toBe("1");
    });
    // The badge should not have the "live" style (that's for Events only).
    const badge = document.querySelector(".nav-badge");
    expect(badge?.className).not.toContain("nav-badge--live");
  });

  it("the Events nav keeps its 'live' badge", () => {
    render(<Shell />);

    // The Events badge should still render "live" with the live style.
    const navItems = document.querySelectorAll(".nav-item");
    const eventsItem = Array.from(navItems).find((item) =>
      item.querySelector(".nav-label")?.textContent === "Events",
    );
    const badge = eventsItem?.querySelector(".nav-badge");
    expect(badge).toBeInTheDocument();
    expect(badge?.textContent).toBe("live");
    expect(badge?.className).toContain("nav-badge--live");
  });

  it("shows '0' for the Displays badge when the loaded snapshot has zero displays", async () => {
    const { getState } = await import("../api/client");
    vi.mocked(getState).mockResolvedValue(ZERO_DISPLAY_STATE);

    render(<Shell />);

    await waitFor(() => {
      const badge = document.querySelector(".nav-badge");
      expect(badge).toBeInTheDocument();
      expect(badge?.textContent).toBe("0");
    });
  });

  it("renders the GitHub repository link in the sidebar footer", () => {
    render(<Shell />);

    const link = document.querySelector(".sidebar-footer .footer-github");
    expect(link).toBeInTheDocument();
    expect(link?.getAttribute("href")).toBe("https://github.com/legion-works/dormant");
    expect(link?.getAttribute("target")).toBe("_blank");
    expect(link?.getAttribute("rel")).toContain("noopener");
    expect(link?.getAttribute("aria-label")).toBe("GitHub repository");
  });
});
