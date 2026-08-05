import { StrictMode } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { SectionRailProvider } from "../app/config/SectionRailContext";
import { useRegisterSection, useRegisteredSections } from "../app/config/sectionRail";

function Section({ id, title }: { id: string; title: string }) {
  useRegisterSection(id, title, null);
  return <div>{title}</div>;
}

function Registry() {
  return <output role="status">{useRegisteredSections().map((section) => section.id).join(",")}</output>;
}

function Fixture({ tab, includeA = true }: { tab: string; includeA?: boolean }) {
  return (
    <SectionRailProvider tab={tab}>
      {includeA && <Section id="a" title="A" />}
      <Section id="b" title="B" />
      <Registry />
    </SectionRailProvider>
  );
}

describe("SectionRailContext", () => {
  afterEach(cleanup);
  it("registers sections once under StrictMode and removes them on unmount", () => {
    const { rerender } = render(
      <StrictMode>
        <Fixture tab="daemon" />
      </StrictMode>,
    );
    expect(screen.getByRole("status")).toHaveTextContent("a,b");

    rerender(
      <StrictMode>
        <Fixture tab="daemon" includeA={false} />
      </StrictMode>,
    );
    expect(screen.getByRole("status")).toHaveTextContent("b");
  });

  it("clears registrations when the tab changes", () => {
    const { rerender } = render(<Fixture tab="daemon" />);
    expect(screen.getByRole("status")).toHaveTextContent("a,b");
    rerender(<Fixture tab="presence" />);
    expect(screen.getByRole("status")).toBeEmptyDOMElement();
  });
});
