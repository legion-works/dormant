/**
 * Contract test: the pairing wizard's API client helpers (spec §8).
 *
 * POST /api/pair/samsung -> 202 {pair_id}; GET /api/pair/samsung/{id} ->
 * {state, detail}. Mirrors the Content-Type contract test style in
 * client.test.ts/client-apply.test.ts.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { postPairSamsung, getPairStatus } from "../api/client";

describe("postPairSamsung", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("POSTs { host } with Content-Type: application/json and resolves { pair_id }", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ pair_id: "abc123" }), {
        status: 202,
        headers: { "Content-Type": "application/json" },
      }),
    );

    const result = await postPairSamsung("192.0.2.1");
    expect(result).toEqual({ pair_id: "abc123" });

    const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit | undefined];
    expect(url).toBe("/api/pair/samsung");
    expect(init?.method).toBe("POST");
    const headers = init?.headers as Record<string, string> | undefined;
    expect(headers?.["Content-Type"]).toBe("application/json");
    expect(JSON.parse(init?.body as string)).toEqual({ host: "192.0.2.1" });
  });

  it("throws ApiError with status + body on 409 pairing_in_progress", async () => {
    const errorBody = { error: "pairing_in_progress" };
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(errorBody), {
        status: 409,
        headers: { "Content-Type": "application/json" },
      }),
    );

    await expect(postPairSamsung("192.0.2.1")).rejects.toMatchObject({
      status: 409,
      body: errorBody,
    });
  });

  it("throws ApiError with status + body on 403 feature_disabled", async () => {
    const errorBody = { error: "feature_disabled" };
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(errorBody), {
        status: 403,
        headers: { "Content-Type": "application/json" },
      }),
    );

    await expect(postPairSamsung("192.0.2.1")).rejects.toMatchObject({
      status: 403,
      body: errorBody,
    });
  });
});

describe("getPairStatus", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("GETs /api/pair/samsung/{id} and resolves the PairStatus", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ state: "pairing", detail: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );

    const result = await getPairStatus("abc123");
    expect(result).toEqual({ state: "pairing", detail: null });

    const [url] = fetchSpy.mock.calls[0] as [string];
    expect(url).toBe("/api/pair/samsung/abc123");
  });

  it("never resolves a token-shaped field (contract: PairStatus has no token key)", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ state: "paired", detail: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );

    const result = await getPairStatus("abc123");
    expect(result).not.toHaveProperty("token");
  });
});
