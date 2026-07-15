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
  postEmergencyWake,
  postExercise,
  getOperations,
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

describe("postEmergencyWake", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("posts global emergency wake and returns the typed report", async () => {
    fetchSpy.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          paused: true,
          displays: [{ display: "studio", ok: true }],
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );

    await expect(postEmergencyWake()).resolves.toEqual({
      paused: true,
      displays: [{ display: "studio", ok: true }],
    });
    expect(fetchSpy).toHaveBeenCalledWith("/api/emergency-wake", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
  });

  it("preserves emergency wake 409 and 504 statuses", async () => {
    fetchSpy
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "emergency_wake_in_progress" }), {
          status: 409,
          headers: { "Content-Type": "application/json" },
        }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "emergency_wake_report_timeout" }), {
          status: 504,
          headers: { "Content-Type": "application/json" },
        }),
      );

    await expect(postEmergencyWake()).rejects.toMatchObject({ status: 409 });
    await expect(postEmergencyWake()).rejects.toMatchObject({ status: 504 });
  });
});

describe("postExercise / getOperations", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("posts a URL-encoded display exercise and returns the report", async () => {
    const report = {
      display: "main panel",
      pre_phase: "active",
      paused_rules: ["office_blank"],
      steps: [],
    };
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(report), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );

    await expect(postExercise("main panel")).resolves.toEqual(report);
    expect(fetchSpy).toHaveBeenCalledWith("/api/doctor/exercise/main%20panel", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
  });

  it("preserves exercise 409 and report-timeout 504 statuses", async () => {
    fetchSpy
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "exercise_in_progress" }), {
          status: 409,
          headers: { "content-type": "application/json" },
        }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "exercise_report_timeout" }), {
          status: 504,
          headers: { "content-type": "application/json" },
        }),
      );
    await expect(postExercise("main")).rejects.toMatchObject({ status: 409 });
    await expect(postExercise("main")).rejects.toMatchObject({ status: 504 });
  });

  it("gets authoritative web-operation guard status", async () => {
    const status = {
      exercise_in_flight: ["main"],
      emergency_wake_in_flight: true,
    };
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify(status), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );

    await expect(getOperations()).resolves.toEqual(status);
    expect(fetchSpy).toHaveBeenCalledWith("/api/operations", undefined);
  });
});
