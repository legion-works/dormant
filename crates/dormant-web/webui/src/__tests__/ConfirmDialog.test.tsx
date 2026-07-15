import { afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import ConfirmDialog from "../app/components/ConfirmDialog";
import { useConfirmDialog } from "../app/components/useConfirmDialog";

afterEach(cleanup);

describe("ConfirmDialog", () => {
  it("renders an accessible alert dialog and confirms", () => {
    const onConfirm = vi.fn();
    render(
      <ConfirmDialog
        open
        title="Exercise main?"
        description="The panel will visibly go dark and return."
        confirmLabel="Run exercise"
        tone="danger"
        onConfirm={onConfirm}
        onCancel={vi.fn()}
      />,
    );

    expect(screen.getByRole("alertdialog", { name: "Exercise main?" })).toBeInTheDocument();
    const cancel = screen.getByRole("button", { name: "Cancel" });
    const confirm = screen.getByRole("button", { name: "Run exercise" });
    expect(cancel).toHaveFocus();
    fireEvent.keyDown(cancel, { key: "Tab", shiftKey: true });
    expect(confirm).toHaveFocus();
    fireEvent.click(confirm);
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it("cancels on Escape and backdrop click", () => {
    const onCancel = vi.fn();
    render(
      <ConfirmDialog
        open
        title="Force blank main?"
        description="This immediately blanks the panel."
        confirmLabel="Force blank"
        onConfirm={vi.fn()}
        onCancel={onCancel}
      />,
    );

    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledOnce();
    fireEvent.mouseDown(screen.getByTestId("confirm-backdrop"));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  it("deterministically rejects a second confirm while one is pending", async () => {
    function Harness() {
      const { confirm, dialog } = useConfirmDialog();
      const [result, setResult] = useState("pending");
      return (
        <>
          <button
            type="button"
            onClick={async () => {
              const first = confirm({
                title: "First action?",
                description: "First confirmation",
                confirmLabel: "Confirm first",
              });
              const second = confirm({
                title: "Second action?",
                description: "Second confirmation",
                confirmLabel: "Confirm second",
              });
              setResult(JSON.stringify(await Promise.all([first, second])));
            }}
          >
            open twice
          </button>
          <output>{result}</output>
          {dialog}
        </>
      );
    }

    render(<Harness />);
    fireEvent.click(screen.getByRole("button", { name: "open twice" }));
    expect(screen.getByRole("alertdialog", { name: "First action?" })).toBeInTheDocument();
    expect(screen.queryByText("Second action?")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Confirm first" }));
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("[true,false]"));
  });
});
