import { describe, it, expect } from "vitest";
import { parseWorkspaceVersion } from "../../scripts/workspace-version";

const FIXTURE = `[workspace]
resolver = "2"
members = [
  "crates/dormant-core",
  "crates/dormant-web",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "MIT OR Apache-2.0"
repository = "https://github.com/legion-works/dormant"
`;

describe("parseWorkspaceVersion", () => {
  it("extracts the version from [workspace.package]", () => {
    expect(parseWorkspaceVersion(FIXTURE)).toBe("0.1.0");
  });

  it("reflects a changed workspace version (parser tracks the source, not a cached literal)", () => {
    const bumped = FIXTURE.replace('version = "0.1.0"', 'version = "9.9.9"');

    expect(parseWorkspaceVersion(bumped)).toBe("9.9.9");
    expect(parseWorkspaceVersion(bumped)).not.toBe(parseWorkspaceVersion(FIXTURE));
  });

  it("throws a clear error when [workspace.package] is missing", () => {
    const noTable = `[package]\nname = "dormant"\nversion = "0.1.0"\n`;

    expect(() => parseWorkspaceVersion(noTable)).toThrow(/workspace\.package/);
  });

  it("throws a clear error when version is absent from [workspace.package]", () => {
    const noVersion = `[workspace.package]\nedition = "2024"\n`;

    expect(() => parseWorkspaceVersion(noVersion)).toThrow(/version/);
  });

  it("stops scanning at the next table so it doesn't pick up an unrelated version field", () => {
    const wrongTableVersion = `[workspace.package]
edition = "2024"

[dependencies]
version = "1.2.3"
`;

    expect(() => parseWorkspaceVersion(wrongTableVersion)).toThrow(/version/);
  });
});
