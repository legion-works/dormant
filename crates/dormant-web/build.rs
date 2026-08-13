//! Guard against shipping the placeholder SPA.
//!
//! `webui/dist/index.html` is a tracked placeholder so that `cargo
//! check`/`test`/`clippy` work without a Node toolchain — `rust-embed`
//! needs the folder to exist, and the Rust suite only asserts that the
//! SPA fallback serves `text/html`.
//!
//! The hazard is that `rust-embed` bakes in whatever is on disk at
//! compile time. A release build performed without running the Vite
//! build first produces a daemon that serves a blank page, and nothing
//! downstream notices: the binary runs, the routes answer, the tests
//! passed. CI never catches it because no CI job builds `--release`
//! (the release pipeline runs `npm run build` before cargo, and the
//! debug jobs are content with the placeholder).
//!
//! So the check is scoped to exactly the case that has shipped a blank
//! UI three times: a RELEASE build with the placeholder still in place.
//! Debug builds keep working untouched.

use std::path::Path;

/// Marker text carried by the checked-in placeholder. Matching on this
/// rather than a size or hash keeps the check readable and survives
/// edits to the placeholder's wording around it.
const PLACEHOLDER_MARKER: &str = "PLACEHOLDER";

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("webui/dist");
    let index = dist.join("index.html");

    // Rebuild whenever the embedded bundle changes, so a stale success
    // is not cached past an `npm run build`.
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-changed={}", index.display());

    // Only release builds are gated. Debug builds are the dev/CI path
    // and are expected to carry the placeholder.
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if profile != "release" {
        return;
    }

    let contents = match std::fs::read_to_string(&index) {
        Ok(contents) => contents,
        Err(error) => {
            panic!(
                "\n\n\
                 dormant-web: cannot read {}: {error}\n\n\
                 The embedded web UI is missing. Build it before a release build:\n\
                 \n    cd crates/dormant-web/webui && npm ci && npm run build\n\n\
                 Or use the canonical local deploy, which does this for you:\n\
                 \n    bash scripts/deploy-local.sh\n\n",
                index.display()
            );
        }
    };

    if contents.contains(PLACEHOLDER_MARKER) {
        panic!(
            "\n\n\
             dormant-web: refusing to embed the placeholder SPA into a release build.\n\n\
             {} is still the checked-in placeholder, so this binary would serve a\n\
             blank page instead of the dashboard. Build the real bundle first:\n\
             \n    cd crates/dormant-web/webui && npm ci && npm run build\n\n\
             Or use the canonical local deploy, which does this for you:\n\
             \n    bash scripts/deploy-local.sh\n\n",
            index.display()
        );
    }
}
