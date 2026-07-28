import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useRef } from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SectionRail } from "../app/config/SectionRail";
import { SectionRailProvider, useRegisterSection } from "../app/config/SectionRailContext";

function RegisteredSections() {
  const anchor = useRef<HTMLDivElement>(null);
  useRegisterSection("rules", "Rules", anchor);
  return <div ref={anchor} />;
}

function MultipleRegisteredSections() {
  const displays = useRef<HTMLDivElement>(null);
  const wear = useRef<HTMLDivElement>(null);
  useRegisterSection("displays", "Displays", displays);
  useRegisterSection("wear", "Wear", wear);
  return (
    <>
      <div
        ref={(element) => {
          displays.current = element;
          if (element) element.scrollIntoView = vi.fn();
        }}
      />
      <div
        ref={(element) => {
          wear.current = element;
          if (element) element.scrollIntoView = vi.fn();
        }}
      />
    </>
  );
}

describe("SectionRail", () => {
  beforeEach(() => {
    window.matchMedia = vi.fn().mockReturnValue({ matches: false });
  });

  afterEach(() => {
    cleanup();
    window.location.hash = "";
  });

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

  it("preserves the config route and round-trips the section fragment", () => {
    window.location.hash = "#/config/displays";
    render(
      <SectionRailProvider tab="presence">
        <MultipleRegisteredSections />
        <SectionRail />
      </SectionRailProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Displays" }));

    expect(window.location.hash).toBe("#/config/displays#config-section-displays");
    expect(window.location.hash.replace(/^#\/?/, "").split("/")[0]).toBe("config");
    expect(window.location.hash.split("#")[2]).toBe("config-section-displays");
  });

  it("replaces an existing section fragment instead of accumulating hashes", () => {
    window.location.hash = "#/config/displays#config-section-wear";
    render(
      <SectionRailProvider tab="presence">
        <MultipleRegisteredSections />
        <SectionRail />
      </SectionRailProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Displays" }));

    expect(window.location.hash).toBe("#/config/displays#config-section-displays");
    expect(window.location.hash.split("#")).toHaveLength(3);
  });
});
