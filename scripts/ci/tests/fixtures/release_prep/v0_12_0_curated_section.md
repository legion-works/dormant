## [0.12.0] - 2026-08-06

### Breaking

- Disable the old `dormant.service` unit and enable `app-dormant.service` instead when upgrading ([#233](https://github.com/legion-works/dormant/issues/233), [#241](https://github.com/legion-works/dormant/pull/241)).

### Highlights

**Source-gated TV wear sampling** — attribute HDMI-connected Samsung TV wear from local frames only while the TV is showing that input. See [the Active wear sampling chapter](./docs/src/active-wear-sampling.md) ([#231](https://github.com/legion-works/dormant/pull/231)).

**TV app-overlay gating** — pause spatial attribution when a watched Tizen app owns the panel even though the TV still reports the configured HDMI input. Configure `watched_apps` per display; a visible app degrades the interval to uniform attribution ([#232](https://github.com/legion-works/dormant/issues/232)).

### Fixed

- `dormantctl doctor` no longer fails every display's wear-sampling probe when the active-sampling config lists more than one display. The probe compared each display's consent binding against `first_sampled_display()`, which returns nothing under a plural config, so every bound display read as a mismatch and reported a binding error even while sampling worked correctly ([#234](https://github.com/legion-works/dormant/issues/234)).
- Fix a false alarm in the wear heat map: a panel with mean exposure below 1 h no longer renders a full-red hotspot for a +20% relative deviation. Below the 1-hour floor the map renders a neutral colour and displays an "insufficient wear data (<1h mean)" caption instead ([#216](https://github.com/legion-works/dormant/issues/216), [#237](https://github.com/legion-works/dormant/pull/237)).
- MQTT hook actions now publish through the configured sensor-plane broker and credentials, including after configuration reloads ([#230](https://github.com/legion-works/dormant/issues/230), [#238](https://github.com/legion-works/dormant/pull/238)).

### Changed

- Add an injectable MQTT event-loop seam so subscription lifecycle behavior can be tested without a broker, pinning the #213 subscription contract ([#215](https://github.com/legion-works/dormant/issues/215), [#242](https://github.com/legion-works/dormant/pull/242)).
- A failed AUR publish no longer marks an otherwise-successful release run as failed. The AUR's git endpoint goes offline for maintenance and restricts pushes during upstream security incidents, neither of which reflects on the release itself. The published AUR version is verified out of band rather than inferred from the job's exit code ([#229](https://github.com/legion-works/dormant/pull/229)).

