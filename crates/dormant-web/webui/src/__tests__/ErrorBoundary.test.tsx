import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ErrorBoundary } from "../app/ErrorBoundary";

describe("ErrorBoundary", () => {
  it("shows a visible dashboard error when a child throws", () => {
    function BrokenChild(): never {
      throw new Error("render failed");
    }

    render(
      <ErrorBoundary>
        <BrokenChild />
      </ErrorBoundary>,
    );

    expect(screen.getByText(/The dashboard hit an error: render failed/)).toBeInTheDocument();
    expect(screen.getByText(/The daemon is still running/)).toBeInTheDocument();
  });
});
