/**
 * CreateEntityForm — the shared entity-create form used by all four CRUD
 * sections (spec §7, T6). Per-collection field set mirrors
 * `CREATABLE_FIELDS` (entityCrud.ts); the sensor type discriminator
 * conditionally shows the mqtt/ha/usb-ld2410 subset.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import CreateEntityForm from "../app/config/CreateEntityForm";

afterEach(() => cleanup());

describe("CreateEntityForm — id hygiene live feedback", () => {
  it("shows no error for an empty id (not yet typed)", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByText(/reserved|charset|letter|empty/i)).not.toBeInTheDocument();
  });

  it("rejects a reserved id live, before submit", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "weights" } });
    expect(screen.getByText(/reserved/i)).toBeInTheDocument();
  });

  it("rejects a bad-charset id live", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "My Sensor" } });
    expect(screen.getByText(/lowercase|characters/i)).toBeInTheDocument();
  });

  it("rejects an id already present in existingIds", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={["desk"]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "desk" } });
    expect(screen.getByText(/already exists/i)).toBeInTheDocument();
  });

  it("Create button is disabled until a valid, non-colliding id is entered", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={["desk"]} onCreate={() => {}} onCancel={() => {}} />,
    );
    const createBtn = screen.getByRole("button", { name: /create/i });
    expect(createBtn).toBeDisabled();

    fireEvent.change(screen.getByLabelText("id"), { target: { value: "desk" } });
    expect(createBtn).toBeDisabled();

    fireEvent.change(screen.getByLabelText("id"), { target: { value: "new-desk" } });
    expect(createBtn).not.toBeDisabled();
  });
});

describe("CreateEntityForm — sensors (per-type conditional rendering)", () => {
  it("defaults to mqtt fields visible, ha/usb fields hidden", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.getByLabelText("broker_url")).toBeInTheDocument();
    expect(screen.getByLabelText("topic")).toBeInTheDocument();
    expect(screen.queryByLabelText("url")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("entity")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("port")).not.toBeInTheDocument();
  });

  it("switching type to ha shows url/entity, hides mqtt fields", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("type"), { target: { value: "ha" } });
    expect(screen.getByLabelText("url")).toBeInTheDocument();
    expect(screen.getByLabelText("entity")).toBeInTheDocument();
    expect(screen.queryByLabelText("broker_url")).not.toBeInTheDocument();
  });

  it("switching type to usb-ld2410 shows port/baud", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("type"), { target: { value: "usb-ld2410" } });
    expect(screen.getByLabelText("port")).toBeInTheDocument();
    expect(screen.getByLabelText("baud")).toBeInTheDocument();
  });

  it("submitting an mqtt sensor calls onCreate with id + value including type", () => {
    let captured: [string, Record<string, unknown>] | null = null;
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        onCreate={(id, value) => { captured = [id, value]; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "new-desk" } });
    fireEvent.change(screen.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(screen.getByLabelText("topic"), { target: { value: "sensors/desk" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured).not.toBeNull();
    const [id, value] = captured!;
    expect(id).toBe("new-desk");
    expect(value).toMatchObject({
      type: "mqtt",
      broker_url: "tcp://mqtt:1883",
      topic: "sensors/desk",
    });
  });
});

describe("CreateEntityForm — zones", () => {
  it("renders mode select and a members multi-select from sensorIds", () => {
    render(
      <CreateEntityForm
        collection="zones"
        existingIds={[]}
        sensorIds={["desk-mmwave", "room-pir"]}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    expect(screen.getByLabelText("mode")).toBeInTheDocument();
    expect(screen.getByLabelText("members: desk-mmwave")).toBeInTheDocument();
    expect(screen.getByLabelText("members: room-pir")).toBeInTheDocument();
  });

  it("submitting selects members and mode into the create value", () => {
    let captured: [string, Record<string, unknown>] | null = null;
    render(
      <CreateEntityForm
        collection="zones"
        existingIds={[]}
        sensorIds={["desk-mmwave", "room-pir"]}
        onCreate={(id, value) => { captured = [id, value]; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "new-zone" } });
    fireEvent.click(screen.getByLabelText("members: desk-mmwave"));
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured).not.toBeNull();
    const [id, value] = captured!;
    expect(id).toBe("new-zone");
    expect(value.members).toEqual(["desk-mmwave"]);
  });
});

describe("CreateEntityForm — displays", () => {
  it("renders a controllers multi-select (no type discriminator)", () => {
    render(
      <CreateEntityForm collection="displays" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByLabelText("type")).not.toBeInTheDocument();
    expect(screen.getByLabelText("controllers: samsung-tizen")).toBeInTheDocument();
  });

  it("never renders blank_command/wake_command inputs (structurally excluded)", () => {
    render(
      <CreateEntityForm collection="displays" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByLabelText("blank_command")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("wake_command")).not.toBeInTheDocument();
  });

  it("seeds host + controllers from initialFields (pairing wizard hand-off, spec §8.3)", () => {
    let captured: [string, Record<string, unknown>] | null = null;
    render(
      <CreateEntityForm
        collection="displays"
        existingIds={[]}
        initialFields={{ host: "192.0.2.42", controllers: ["samsung-tizen"] }}
        onCreate={(id, value) => { captured = [id, value]; }}
        onCancel={() => {}}
      />,
    );
    expect(screen.getByLabelText("host")).toHaveValue("192.0.2.42");
    expect((screen.getByLabelText("controllers: samsung-tizen") as HTMLInputElement).checked).toBe(true);

    fireEvent.change(screen.getByLabelText("id"), { target: { value: "paired-tv" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured).not.toBeNull();
    const [, value] = captured!;
    expect(value.host).toBe("192.0.2.42");
    expect(value.controllers).toEqual(["samsung-tizen"]);
  });
});

describe("CreateEntityForm — rules", () => {
  it("renders zone select, displays multi-select, and inhibitors multi-select from VALID_INHIBITORS", () => {
    render(
      <CreateEntityForm
        collection="rules"
        existingIds={[]}
        zoneIds={["office", "hallway"]}
        displayIds={["aoc-main"]}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    expect(screen.getByLabelText("zone")).toBeInTheDocument();
    expect(screen.getByLabelText("displays: aoc-main")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: user-activity")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: audio-playback")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: call")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: manual-pause")).toBeInTheDocument();
  });
});
