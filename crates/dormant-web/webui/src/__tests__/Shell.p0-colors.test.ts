/**
 * P0 color-role pin — jsdom does not compute `color-mix()`/CSS custom
 * properties, so the semantic flip (nav-active green+bordered, reload
 * cyan chrome) is pinned by reading Shell.css's source text and
 * asserting the CORRECT variable appears in each rule block, rather
 * than a computed-style assertion. Mirrors ds.css's own stated
 * convention: "present / awake / active = --success. Legion cyan =
 * system chrome only (reload, clock, live pulse)."
 */
/// <reference types="node" />

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const SHELL_CSS_PATH = resolve(import.meta.dirname, "../app/Shell.css");

/** Extract a `.selector { ... }` block's raw body from the stylesheet
 * text (first match only — every rule pinned here is declared once). */
function ruleBody(css: string, selector: string): string {
  const escaped = selector.replace(/[.]/g, "\\.");
  const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`));
  if (!match) throw new Error(`rule ${selector} not found in Shell.css`);
  return match[1];
}

describe("Shell.css P0 color-role pin", () => {
  const css = readFileSync(SHELL_CSS_PATH, "utf8");

  it(".nav-item--active is green (tinted + bordered), never cyan", () => {
    const body = ruleBody(css, ".nav-item--active");
    expect(body).toMatch(/var\(--success-muted\)/);
    expect(body).toMatch(/border:\s*1px solid/);
    expect(body).toMatch(/var\(--success\)/);
    expect(body).not.toMatch(/var\(--accent-muted\)/);
  });

  it(".topbar-reload is cyan chrome, never green", () => {
    const body = ruleBody(css, ".topbar-reload");
    expect(body).toMatch(/color:\s*var\(--accent\)/);
    expect(body).toMatch(/background-color:\s*var\(--accent-muted\)/);
    expect(body).toMatch(/var\(--accent\)/);
    expect(body).not.toMatch(/var\(--success\)/);
  });

  it(".topbar-reload:hover uses the cyan hover tint, never green", () => {
    const body = ruleBody(css, ".topbar-reload:hover");
    expect(body).toMatch(/var\(--accent\)/);
    expect(body).not.toMatch(/var\(--success\)/);
  });
});
