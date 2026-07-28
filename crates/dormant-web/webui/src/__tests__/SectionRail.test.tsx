import { describe, expect, it } from "vitest";
import { useRef } from "react";
import { render, screen } from "@testing-library/react";
import { SectionRail } from "../app/config/SectionRail";
import { SectionRailProvider, useRegisterSection } from "../app/config/SectionRailContext";

function RegisteredSections() {
  const anchor = useRef<HTMLDivElement>(null);
  useRegisterSection("rules", "Rules", anchor);
  return <div ref={anchor} />;
}

describe("SectionRail", () => {
  it("renders registered section titles", () => {
    render(
      <SectionRailProvider tab="presence">
        <RegisteredSections />
        <SectionRail />
      </SectionRailProvider>,
    );
    expect(screen.getByRole("navigation", { name: "Config sections" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Rules" })).toBeInTheDocument();
  });
});
