/**
 * CreateEntityForm — guided-entity-creation affordances.
 *
 * Covers (per the brief):
 *   2.i  every creatable field renders a one-line help text and an example placeholder,
 *        grounded in `docs/src/configuration.md`, `sensors.md`, `displays.md`.
 *   2.ii sensor `type`, sensor `kind`, zone `mode`, zone `unavailable_policy`
 *        are rendered as selects (closed value sets, sourced from
 *        `crates/dormant-core/src/config/schema.rs` and `zone.rs`).
 *   2.iii the sensor create form gets an optional "Add to zone" select
 *         whose submit emits the CreateEntity patch PLUS a Set patch on
 *         `zones.<zoneId>.members` (the server's `config_patch.rs` allows
 *         this since `members` is a known path and the value is an array).
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, within } from "@testing-library/react";
import CreateEntityForm from "../app/config/CreateEntityForm";
import { FIELD_HELP, FIELD_EXAMPLE, CREATABLE_FIELDS, CRUD_COLLECTIONS } from "../app/config/entityCrud";
import type { ConfigPatch } from "../api/types";

afterEach(() => cleanup());

describe("CreateEntityForm — guided help / placeholders for every creatable field (2.i)", () => {
  it("sensor with mqtt type renders the shared + mqtt help texts", () => {
    const form = render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    const txt = form.container.textContent ?? "";
    for (const field of ["type", "broker_url", "topic", "field", "payload_on", "payload_off", "kind", "hold_time", "stale_timeout"]) {
      const help = FIELD_HELP.sensors[field];
      expect(help, `FIELD_HELP.sensors.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("sensor with ha type renders the ha-only help texts", () => {
    const form = render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("type"), { target: { value: "ha" } });
    const txt = form.container.textContent ?? "";
    for (const field of ["url", "entity"]) {
      const help = FIELD_HELP.sensors[field];
      expect(help, `FIELD_HELP.sensors.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("sensor with usb-ld2410 type renders the usb-only help texts", () => {
    const form = render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("type"), { target: { value: "usb-ld2410" } });
    const txt = form.container.textContent ?? "";
    for (const field of ["port", "baud"]) {
      const help = FIELD_HELP.sensors[field];
      expect(help, `FIELD_HELP.sensors.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("zones renders mode / members / unavailable_policy help", () => {
    const form = render(
      <CreateEntityForm
        collection="zones"
        existingIds={[]}
        sensorIds={["s1"]}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    const txt = form.container.textContent ?? "";
    for (const field of ["mode", "members", "unavailable_policy"]) {
      const help = FIELD_HELP.zones[field];
      expect(help, `FIELD_HELP.zones.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("displays in private mode renders the non-shared help texts", () => {
    const form = render(
      <CreateEntityForm collection="displays" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    const txt = form.container.textContent ?? "";
    for (const field of [
      "controllers", "blank_mode", "output", "ddc_display", "wol_mac", "host",
      "restore_brightness", "treat_unreachable_as_blanked", "command_timeout",
    ]) {
      const help = FIELD_HELP.displays[field];
      expect(help, `FIELD_HELP.displays.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("displays in shared mode additionally renders shared_*_code help", () => {
    const form = render(
      <CreateEntityForm collection="displays" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    fireEvent.change(screen.getByLabelText("scope"), { target: { value: "shared" } });
    const txt = form.container.textContent ?? "";
    for (const field of ["shared_input_code", "shared_input_write_code", "shared_peer_input_code", "shared_peer_input_write_code"]) {
      const help = FIELD_HELP.displays[field];
      expect(help, `FIELD_HELP.displays.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("rules renders every rule-creatable help text", () => {
    const form = render(
      <CreateEntityForm
        collection="rules"
        existingIds={[]}
        zoneIds={["office"]}
        displayIds={["d1"]}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    const txt = form.container.textContent ?? "";
    for (const field of CREATABLE_FIELDS.rules) {
      const help = FIELD_HELP.rules[field];
      expect(help, `FIELD_HELP.rules.${field} missing`).toBeTruthy();
      expect(txt).toContain(help);
    }
  });

  it("FIELD_HELP/FIELD_EXAMPLE every collection has no stray entries (no off-creatable keys)", () => {
    for (const c of CRUD_COLLECTIONS) {
      for (const k of Object.keys(FIELD_HELP[c])) expect(CREATABLE_FIELDS[c]).toContain(k);
      for (const k of Object.keys(FIELD_EXAMPLE[c])) expect(CREATABLE_FIELDS[c]).toContain(k);
    }
  });
});

describe("CreateEntityForm — closed-set selects (2.ii)", () => {
  it("sensor type is a select with exactly mqtt, ha, usb-ld2410", () => {
    render(<CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />);
    const t = screen.getByLabelText("type") as HTMLSelectElement;
    const opts = Array.from(t.options).map((o) => o.value);
    expect(opts).toEqual(["mqtt", "ha", "usb-ld2410"]);
  });

  it("sensor kind is a select with exactly presence, motion", () => {
    render(<CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />);
    const kindSel = screen.getByLabelText("kind") as HTMLSelectElement;
    const opts = Array.from(kindSel.options).map((o) => o.value);
    expect(opts).toEqual(["presence", "motion"]);
  });

  it("zone mode is a select with exactly any, all, quorum, weighted", () => {
    render(<CreateEntityForm collection="zones" existingIds={[]} sensorIds={["s1"]} onCreate={() => {}} onCancel={() => {}} />);
    const m = screen.getByLabelText("mode") as HTMLSelectElement;
    const opts = Array.from(m.options).map((o) => o.value);
    expect(opts).toEqual(["any", "all", "quorum", "weighted"]);
  });

  it("zone unavailable_policy defaults to present and is a select with exactly present, absent", () => {
    render(<CreateEntityForm collection="zones" existingIds={[]} sensorIds={["s1"]} onCreate={() => {}} onCancel={() => {}} />);
    const sel = screen.getByLabelText("unavailable_policy") as HTMLSelectElement;
    expect(sel.value).toBe("present");
    const opts = Array.from(sel.options).map((o) => o.value);
    expect(opts).toEqual(["present", "absent"]);
  });
});

describe("CreateEntityForm — Add to zone (sensor, 2.iii)", () => {
  it("does NOT render Add to zone if zoneMembers is omitted", () => {
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office"]}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    expect(screen.queryByLabelText(/add to zone/i)).toBeNull();
  });

  it("renders Add to zone as a select with empty placeholder plus each available zone, defaulting to empty", () => {
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office", "lounge"]}
        zoneMembers={{ office: ["s1"], lounge: [] }}
        onCreate={() => {}}
        onCancel={() => {}}
      />,
    );
    const sel = screen.getByLabelText(/add to zone/i) as HTMLSelectElement;
    expect(sel.value).toBe("");
    const opts = Array.from(sel.options).map((o) => o.value);
    expect(opts).toEqual(["", "office", "lounge"]);
  });

  it("submitting with no zone selected emits only the CreateEntity patch (no members patch)", () => {
    let captured: [{ id: string; value: Record<string, unknown> }, ConfigPatch[] | undefined] | undefined;
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office"]}
        zoneMembers={{ office: ["s1"] }}
        onCreate={(id, value, extra) => { captured = [{ id, value }, extra]; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "desk-radar" } });
    fireEvent.change(screen.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(screen.getByLabelText("topic"), { target: { value: "sensors/desk" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured).toBeDefined();
    expect(captured![0].id).toBe("desk-radar");
    expect(captured![1]).toBeUndefined();
  });

  it("submitting with a zone selected emits the CreateEntity patch AND a Set patch on zones.<id>.members that APPENDS the new sensor id", () => {
    let captured: [{ id: string; value: Record<string, unknown> }, ConfigPatch[] | undefined] | undefined;
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office"]}
        zoneMembers={{ office: ["s1"] }}
        onCreate={(id, value, extra) => { captured = [{ id, value }, extra]; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "desk-radar" } });
    fireEvent.change(screen.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(screen.getByLabelText("topic"), { target: { value: "sensors/desk" } });

    fireEvent.change(screen.getByLabelText(/add to zone/i), { target: { value: "office" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured).toBeDefined();
    expect(captured![0].id).toBe("desk-radar");
    const extras = captured![1];
    expect(extras).toBeDefined();
    expect(extras!).toHaveLength(1);
    expect(extras![0]).toEqual({
      op: "set",
      path: ["zones", "office", "members"],
      value: ["s1", "desk-radar"],
    });
  });

  it("does NOT duplicate the sensor id in members when it already exists", () => {
    let captured: [{ id: string; value: Record<string, unknown> }, ConfigPatch[] | undefined] | undefined;
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office"]}
        zoneMembers={{ office: ["s1", "desk-radar"] }}
        onCreate={(id, value, extra) => { captured = [{ id, value }, extra]; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "desk-radar" } });
    fireEvent.change(screen.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(screen.getByLabelText("topic"), { target: { value: "sensors/desk" } });
    fireEvent.change(screen.getByLabelText(/add to zone/i), { target: { value: "office" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(captured![1]).toBeUndefined();
  });

  it("Add to zone is sensors-only (not rendered for zones, displays, rules)", () => {
    const { rerender } = render(
      <CreateEntityForm collection="zones" existingIds={[]} sensorIds={["s1"]} zoneMembers={{ office: [] }} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByLabelText(/add to zone/i)).toBeNull();
    rerender(
      <CreateEntityForm collection="displays" existingIds={[]} zoneMembers={{ office: [] }} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByLabelText(/add to zone/i)).toBeNull();
    rerender(
      <CreateEntityForm collection="rules" existingIds={[]} zoneIds={["z1"]} displayIds={["d1"]} zoneMembers={{ office: [] }} onCreate={() => {}} onCancel={() => {}} />,
    );
    expect(screen.queryByLabelText(/add to zone/i)).toBeNull();
  });

  it("extra patch adds the new sensor id (NOT any other id) to the chosen zone's members", () => {
    let extras: ConfigPatch[] | undefined;
    render(
      <CreateEntityForm
        collection="sensors"
        existingIds={[]}
        zoneIds={["office", "lounge"]}
        zoneMembers={{ office: [], lounge: ["existing"] }}
        onCreate={(_id, _value, extra) => { extras = extra; }}
        onCancel={() => {}}
      />,
    );
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "brand-new" } });
    fireEvent.change(screen.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(screen.getByLabelText("topic"), { target: { value: "sensors/new" } });
    fireEvent.change(screen.getByLabelText(/add to zone/i), { target: { value: "lounge" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    expect(extras).toHaveLength(1);
    expect(extras![0]).toEqual({
      op: "set",
      path: ["zones", "lounge", "members"],
      value: ["existing", "brand-new"],
    });
  });

  it("uses within() to scope lookups inside the create form (regression / sanity)", () => {
    render(
      <CreateEntityForm collection="sensors" existingIds={[]} onCreate={() => {}} onCancel={() => {}} />,
    );
    const form = within(screen.getByTestId("create-sensors-form"));
    expect(form.getByLabelText("id")).toBeInTheDocument();
  });
});
