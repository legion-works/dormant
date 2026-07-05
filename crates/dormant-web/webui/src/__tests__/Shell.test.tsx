/**
 * Smoke test: Shell renders the sidebar nav with all five views.
 *
 * The DS CSS imports are NOT resolved in the jsdom test environment
 * (they import from the design directory).  The test verifies the
 * component DOM structure — visual output is checked in a real browser
 * during dev + the `.dc.html` prototypes serve as the pixel reference.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import Shell from "../app/Shell";

// Ensure clean DOM between tests (vitest runs tests in same jsdom window).
afterEach(() => cleanup());

describe("Shell", () => {
  it("renders the sidebar navigation with all five views", () => {
    render(<Shell />);

    // Brand block
    expect(screen.getByText("dormant")).toBeInTheDocument();
    expect(screen.getByText("v0.1.0 · pre-alpha")).toBeInTheDocument();

    // Nav items — scoped to .sidebar-nav so they don't collide with content area
    const navEl = document.querySelector(".sidebar-nav")!;
    const navLabels = ["Dashboard", "Displays", "Events", "Config", "Doctor"];
    for (const label of navLabels) {
      const items = navEl.querySelectorAll(".nav-label");
      const found = Array.from(items).some((el) => el.textContent === label);
      expect(found).toBe(true);
    }

    // Top bar title defaults to Dashboard
    const title = document.querySelector(".topbar-title");
    expect(title?.textContent).toBe("Dashboard");
  });

  it("shows the reload button and clock", () => {
    render(<Shell />);

    // Reload button — scoped to .topbar
    const reload = document.querySelector(".topbar-reload");
    expect(reload).toBeInTheDocument();
    expect(reload?.textContent).toContain("Reload");

    // The clock is present (format HH:MM:SS — verify it renders a string with colons)
    const clockEl = document.querySelector(".topbar-clock");
    expect(clockEl).toBeInTheDocument();
    expect(clockEl?.textContent).toMatch(/\d{2}:\d{2}:\d{2}/);
  });
});
