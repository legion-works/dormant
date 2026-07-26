/**
 * Input-filter section — the `[input_filter]` TOML table.
 *
 * Single field: `ignore_devices`, a string-list controlling which
 * Linux input devices the daemon ignores for user-activity detection.
 * Links the `input-filter` doctor probe and states the `input`-group
 * permission requirement.
 */
import type { InputFilterConfig } from "../../api/types";
import { StringListField } from "./fields";
import type { PatchStore } from "./patch";
import FormSection from "./FormSection";

interface InputFilterSectionProps {
  inputFilter?: InputFilterConfig;
  store: PatchStore;
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

export default function InputFilterSection({
  inputFilter = {},
  store,
  onDirty,
  fieldErrors,
}: InputFilterSectionProps) {
  const devices = inputFilter.ignore_devices ?? [];

  return (
    <FormSection title="Input Filter">
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">[input_filter]</span>
        </div>

        <div className="cf-card__fields">
          <div className="cf-field cf-field--row" data-field-id="input_filter.ignore_devices">
          <StringListField
            path={["input_filter", "ignore_devices"]}
            label="ignore_devices"
            value={devices}
            locked={false}
            help="Linux input device paths to exclude from activity detection. The daemon must run in the `input` group (or have uaccess ACL) to read /dev/input/event* nodes."
            placeholder="e.g. /dev/input/event3"
            error={fieldErrors["input_filter.ignore_devices"]}
            onEdit={(_, v) => {
              store.trackEdit(["input_filter", "ignore_devices"], v);
              onDirty();
            }}
          />
          </div>

          <span className="cf-field__hint">
            {"Verify current devices: "}
            <a
              href="#/doctor?subject=input-filter"
              style={{
                color: "var(--accent)",
                textDecoration: "underline",
                cursor: "pointer",
              }}
            >
              run doctor input-filter
            </a>
          </span>
        </div>
      </div>
    </FormSection>
  );
}
