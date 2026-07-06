/**
 * Contract test: every POST helper in the API client sends
 * Content-Type: application/json.
 *
 * The security guard (security.rs:60-71) rejects any POST without
 * this header → 415 in production.  This test locks the contract
 * so a future editor can't add a bodyless POST and forget the header.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  postBlank,
  postWake,
  postPause,
  postResume,
  postReload,
  runDoctor,
} from "../api/client";

describe("API client POST helpers", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ status: "ok" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  function expectJsonContentType() {
    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit | undefined];
    const headers = init?.headers as Record<string, string> | undefined;
    const ct = headers?.["Content-Type"] ?? "";
    expect(ct, `${url}: Content-Type header`).toBe("application/json");
  }

  it("postBlank sends Content-Type: application/json", async () => {
    await postBlank("test-display");
    expectJsonContentType();
  });

  it("postWake sends Content-Type: application/json", async () => {
    await postWake("test-display");
    expectJsonContentType();
  });

  it("postPause sends Content-Type: application/json", async () => {
    await postPause({ rule: "r1", duration_s: 60 });
    expectJsonContentType();
  });

  it("postResume sends Content-Type: application/json", async () => {
    await postResume({ rule: "r1" });
    expectJsonContentType();
  });

  it("postReload sends Content-Type: application/json", async () => {
    await postReload();
    expectJsonContentType();
  });

  it("runDoctor sends Content-Type: application/json", async () => {
    await runDoctor();
    expectJsonContentType();
  });
});
