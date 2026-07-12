/**
 * StringListField tests — the NET-NEW string-list widget added for T7
 * (`crates/dormant-web/webui/src/app/config/fields.tsx`). Mirrors the
 * existing field widgets' prop contract (path/label/value/locked/onEdit)
 * but manages an array of strings: add, remove, edit an existing entry.
 *
 * The widget owns its working array as internal state (seeded from
 * `value`, re-synced only when the `value` reference itself changes) so
 * that sequential add/remove/edit interactions accumulate correctly even
 * though the parent in this test harness never feeds pending-store state
 * back in as `value` — this is the same "controlled-but-self-consistent"
 * shape a real host (AudioSection) relies on.
 */
import { useState } from "react";
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { StringListField } from "../app/config/fields";

afterEach(() => cleanup());

/** Minimal host that mimics a real form: value flows in, onEdit flows out. */
function Harness({ initial, locked = false }: { initial: string[]; locked?: boolean }) {
  const [value, setValue] = useState<string[]>(initial);
  return (
    <StringListField
      path={["audio", "call_roles"]}
      label="call_roles"
      value={value}
      locked={locked}
      onEdit={(_p, v) => setValue(v as string[])}
    />
  );
}

describe("StringListField", () => {
  it("renders existing entries as chips", () => {
    render(<Harness initial={["Communication", "Multimedia"]} />);
    expect(screen.getByDisplayValue("Communication")).toBeInTheDocument();
    expect(screen.getByDisplayValue("Multimedia")).toBeInTheDocument();
  });

  it("add/remove/edit round-trips through onEdit", () => {
    const onEdit = vi.fn();
    let value: string[] = ["Communication"];

    const { rerender } = render(
      <StringListField
        path={["audio", "call_roles"]}
        label="call_roles"
        value={value}
        locked={false}
        onEdit={(_p, v) => { value = v as string[]; onEdit(v); }}
      />,
    );

    // add
    fireEvent.change(screen.getByLabelText("call_roles"), { target: { value: "Notification" } });
    fireEvent.click(screen.getByLabelText("Add call_roles"));
    expect(onEdit).toHaveBeenLastCalledWith(["Communication", "Notification"]);
    rerender(
      <StringListField
        path={["audio", "call_roles"]}
        label="call_roles"
        value={value}
        locked={false}
        onEdit={(_p, v) => { value = v as string[]; onEdit(v); }}
      />,
    );

    // edit the first entry in place
    fireEvent.change(screen.getByDisplayValue("Communication"), { target: { value: "Comms" } });
    expect(onEdit).toHaveBeenLastCalledWith(["Comms", "Notification"]);
    rerender(
      <StringListField
        path={["audio", "call_roles"]}
        label="call_roles"
        value={value}
        locked={false}
        onEdit={(_p, v) => { value = v as string[]; onEdit(v); }}
      />,
    );

    // remove the second entry
    fireEvent.click(screen.getByLabelText("Remove call_roles item 2"));
    expect(onEdit).toHaveBeenLastCalledWith(["Comms"]);
  });

  it("locked: renders read-only, no add/remove controls, disabled inputs", () => {
    render(<Harness initial={["Communication"]} locked />);
    expect(screen.queryByLabelText("Add call_roles")).toBeNull();
    expect(screen.queryByLabelText("Remove call_roles item 1")).toBeNull();
  });
});
