## [0.12.0] - 2026-08-06

### Breaking

- Disable the old `dormant.service` unit and enable `app-dormant.service` instead when upgrading ([#233](https://github.com/legion-works/dormant/issues/233), [#241](https://github.com/legion-works/dormant/pull/241)).

### Highlights

**Source-gated TV wear sampling** — attribute HDMI-connected Samsung TV wear from local frames only while the TV is showing that input. Sample compositor-driven wear on an HDMI-connected Samsung TV without counting local frames while the TV shows another source. See [the Source-gated TV wear sampling chapter](./docs/src/active-wear-sampling.md) ([#231](https://github.com/legion-works/dormant/pull/231)).

**TV app-overlay gating** — pause spatial attribution when a watched Tizen app owns the panel even though the TV still reports the configured HDMI input. Configure `watched_apps` per display; a visible app degrades the interval to uniform attribution ([#232](https://github.com/legion-works/dormant/issues/232)).

