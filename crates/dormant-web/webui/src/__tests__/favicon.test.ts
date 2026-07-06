/**
 * Favicon parse-validity regression test.
 *
 * The favicon SVG is served standalone in browser tab-chrome with no
 * stylesheet context, so literal hex fills are unavoidable — but the
 * file MUST remain well-formed XML so browsers can decode it.
 */
/// <reference types="node" />

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const FAVICON_PATH = resolve(import.meta.dirname, "../../public/favicon.svg");

describe("favicon.svg", () => {
  it("parses as valid SVG without parser errors", () => {
    const svg = readFileSync(FAVICON_PATH, "utf8");

    const doc = new DOMParser().parseFromString(svg, "image/svg+xml");
    const parseError = doc.querySelector("parsererror");

    expect(parseError).toBeNull();

    const root = doc.documentElement;
    expect(root.tagName).toBe("svg");
    expect(root.namespaceURI).toBe("http://www.w3.org/2000/svg");
  });
});
