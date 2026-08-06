## [0.12.0] - 2026-08-06

### Fixed
- A failed AUR publish no longer marks an otherwise-successful release run as failed. The AUR's git endpoint goes offline for maintenance and restricts pushes during upstream security incidents, neither of which reflects on the release itself. The published AUR version is verified out of band rather than inferred from the job's exit code. ([#230](https://github.com/legion-works/dormant/issues/230))
- Fix a false alarm in the wear heat map: a panel with mean exposure below 1 h no longer renders a full-red hotspot for a +20% relative deviation. Below the 1-hour floor the map renders a neutral colour and displays an "insufficient wear data (<1h mean)" caption instead. ([#230](https://github.com/legion-works/dormant/issues/230))
- MQTT hook actions now publish through the configured sensor-plane broker and credentials, including after configuration reloads. ([#230](https://github.com/legion-works/dormant/issues/230))
- Ship a `NoDisplay=true` desktop entry and rename the Linux user unit to `app-dormant.service` so portal casting indicators can label active wear-sampling sessions as `dormant`. Users upgrading from `dormant.service` must disable the old unit and enable the new one.
- `dormantctl doctor` no longer fails every display's wear-sampling probe when the active-sampling config lists more than one display. The probe compared each display's consent binding against `first_sampled_display()`, which returns nothing under a plural config, so every bound display read as a mismatch and reported a binding error even while sampling worked correctly.

### Changed
- Add an injectable MQTT event-loop seam so subscription lifecycle behavior can be tested
without a broker.
- the probe is fail-safe toward attribution — an unreachable 8001 endpoint never flips the gate on its own (it degrades to the input-only verdict, mirroring the existing source-gate fail-safe). The two probes ride a single `source_poll_interval` timer; a confirmed `Visible` app short-circuits the cycle so the input read is skipped that tick.


### Added
- A declared compositor output and persisted portal position bind consent to the intended panel. Samsung IP Control source polling falls back to tagged uniform attribution whenever the configured HDMI source is not visible or cannot be read.


### Highlights
**Source-gated TV wear sampling** — attribute HDMI-connected Samsung TV wear from local frames only while the TV is showing that input. Sample compositor-driven wear on an HDMI-connected Samsung TV without counting local frames while the TV shows another source. ([#231](https://github.com/legion-works/dormant/pull/231))
