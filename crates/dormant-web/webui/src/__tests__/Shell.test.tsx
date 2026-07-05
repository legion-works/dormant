import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import Shell from "../app/Shell";

afterEach(() => cleanup());

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
});
