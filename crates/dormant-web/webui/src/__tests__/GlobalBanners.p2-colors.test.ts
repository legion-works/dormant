/**
 * P2 rollback recolor pin — jsdom doesn't compute `color-mix()`, so this
 * pins the semantic (rollback = amber, not red) by reading
 * GlobalBanners.css source text. The failure banner (FailureBanner.css,
 * a separate stylesheet/class) is untouched and stays red — that's the
 * proto's stated distinction (red = broken now, amber = safe-on-LKG).
 */
/// <reference types="node" />

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const GLOBAL_BANNERS_CSS_PATH = resolve(import.meta.dirname, "../app/components/GlobalBanners.css");
const FAILURE_BANNER_CSS_PATH = resolve(import.meta.dirname, "../app/components/FailureBanner.css");

function ruleBody(css: string, selector: string): string {
  const escaped = selector.replace(/[.]/g, "\\.");
  const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`));
  if (!match) throw new Error(`rule ${selector} not found`);
  return match[1];
}

describe("GlobalBanners.css P2 rollback recolor pin", () => {
  const css = readFileSync(GLOBAL_BANNERS_CSS_PATH, "utf8");

  it(".global-banner--rollback is amber, never danger red", () => {
    const body = ruleBody(css, ".global-banner--rollback");
    expect(body).toMatch(/var\(--accent-warm\)/);
    expect(body).not.toMatch(/var\(--danger\)/);
  });

  it(".global-banner--rollback .global-banner__action is amber, never danger red", () => {
    const body = ruleBody(css, ".global-banner--rollback .global-banner__action");
    expect(body).toMatch(/var\(--accent-warm\)/);
    expect(body).not.toMatch(/var\(--danger\)/);
  });
});

describe("FailureBanner.css stays red (unchanged distinction)", () => {
  const css = readFileSync(FAILURE_BANNER_CSS_PATH, "utf8");

  it(".failure-banner keeps the danger-red border/background", () => {
    const body = ruleBody(css, ".failure-banner");
    expect(body).toMatch(/var\(--danger\)/);
  });
});
