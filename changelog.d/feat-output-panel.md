---
kind: capability
surfaces: [daemon, config, docs, webui]
issues: []
---
User can now: pin a display to the panel identity on its video connector, preventing shared-monitor blanking and wear captures from following a reused HDMI port onto a different panel.

Detail: The optional output_panel key matches an EDID identity substring without case sensitivity. Positive mismatch uses shared ownership's loss debounce and skips wear capture; missing identity preserves normal wake behaviour. For non-shared displays only wear sampling is gated.
