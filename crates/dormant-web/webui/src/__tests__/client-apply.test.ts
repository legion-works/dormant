/**
 * Contract test: `postConfigApply` surfaces 409 / 422 error bodies
 * to the caller via typed `ApiError`.  The form's conflict banner and
 * per-error display depend on this; reversing it to a bare Error
 * must FAIL a test.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { postConfigApply, ApiError } from "../api/client";
import type { ApplyResponse } from "../api/types";

describe("postConfigApply", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("resolves a typed ApplyResponse on 200", async () => {
    const body: ApplyResponse = {
      applied: true,
      reload: "reloaded",
    };

    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );

    const result = await postConfigApply({
      fingerprint: "abc",
      patches: [],
    });

    expect(result).toEqual(body);
  });

  it("throws ApiError with status 409 and body {error} on fingerprint mismatch", async () => {
    const errorBody = { error: "config changed on disk" };

    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(errorBody), {
        status: 409,
        headers: { "Content-Type": "application/json" },
      }),
    );

    try {
      await postConfigApply({ fingerprint: "abc", patches: [] });
      expect.fail("expected postConfigApply to throw");
    } catch (e) {
      expect(e).toBeInstanceOf(ApiError);
      const ae = e as ApiError;
      expect(ae.status).toBe(409);
      expect(ae.body).toEqual(errorBody);
    }
  });

  it("throws ApiError with status 422 and body {errors} on validation failure", async () => {
    const errorBody = {
      errors: [
        { what: "invalid duration", detail: "hold_time must be a humantime string" },
        { what: "path_denied", detail: "unknown config path: daemon.typo" },
      ],
    };

    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(errorBody), {
        status: 422,
        headers: { "Content-Type": "application/json" },
      }),
    );

    try {
      await postConfigApply({ fingerprint: "abc", patches: [] });
      expect.fail("expected postConfigApply to throw");
    } catch (e) {
      expect(e).toBeInstanceOf(ApiError);
      const ae = e as ApiError;
      expect(ae.status).toBe(422);
      expect(ae.body).toEqual(errorBody);
    }
  });

  it("sends Content-Type: application/json", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ applied: true, reload: "reloaded" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );

    await postConfigApply({ fingerprint: "abc", patches: [] });

    const [, init] = fetchSpy.mock.calls[0] as [string, RequestInit | undefined];
    const headers = init?.headers as Record<string, string> | undefined;
    const ct = headers?.["Content-Type"] ?? "";
    expect(ct).toBe("application/json");
  });
});
