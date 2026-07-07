//! Screensaver playlist builder — pure logic, no I/O beyond filesystem
//! directory scans.
//!
//! Takes a list of [`ScreensaverSource`] configs from
//! [`dormant_core::config::ScreensaverConfig`] and produces a flat,
//! ordered [`PlaylistItem`] sequence.  The mpv player consumes the
//! resulting items with per-item image durations.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dormant_core::config::schema::ScreensaverSource;

/// Media file extensions recognised by the playlist scanner.
///
/// Case-insensitive — the scanner lowercases every extension before
/// checking this set.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

/// Maximum recursion depth when `ScreensaverSource::recurse` is true.
/// Symlink-cycles are bounded by this depth; the scanner does NOT follow
/// symlinks (`fs::read_dir` returns directory entries with
/// `file_type().is_symlink()` which we skip) so cycles through symlinks
/// are inherently prevented, but a deeply nested real-directory tree
/// is still bounded to avoid unbounded stack growth.
const MAX_RECURSE_DEPTH: u32 = 8;

// ── SplitMix64 — deterministic seeded PRNG (no extra deps) ────────────

/// Return the next pseudo-random `u64` and update `state` in place.
///
/// `SplitMix64` has good statistical properties for shuffling; it is the
/// same algorithm used by Rust's std hash-builder seed mixer, so the
/// output quality is well-characterized.
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

// ── Public types ───────────────────────────────────────────────────────

/// A single playlist entry with an optional per-item image duration.
///
/// When `image_duration` is `None`, the player falls back to the global
/// `image-display-duration` property set at init time.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistItem {
    /// URI or absolute path to the media file.
    pub uri: String,
    /// Overrides the global image duration for THIS item only.
    /// `None` means "use the global default".
    pub image_duration: Option<Duration>,
}

// ── Public API ─────────────────────────────────────────────────────────

/// Build a flat playlist from the configured sources.
///
/// # Ordering guarantee
///
/// Sources are processed in config-file order.  Within each source:
///
/// 1. If `path` is set → scan the directory (recursively if
///    `recurse=true`) and collect media files sorted
///    lexicographically.  Shuffle if `shuffle=true`.
/// 2. If `urls` is non-empty → add them in listed order.
///
/// If both `path` and `urls` are set, path items come first,
/// then urls (validation that they should be exclusive is a
/// config-time check, not this function's concern).
///
/// If neither is set, the source contributes no items (the
/// player's empty-playlist guard handles fall-through).
///
/// # Seeding
///
/// `seed` drives the shuffle: `Some(s)` gives reproducible
/// results; `None` seeds from `SystemTime::now()` nanos so each
/// daemon restart produces a fresh shuffle.
///
/// # Logging
///
/// Emits `screensaver_playlist_built{source_count, item_count}`.
#[must_use]
pub fn build_playlist(sources: &[ScreensaverSource], seed: Option<u64>) -> Vec<PlaylistItem> {
    let seed = seed.unwrap_or_else(|| {
        #[allow(clippy::cast_possible_truncation)]
        {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
        }
    });
    let mut rng = seed;

    let mut items: Vec<PlaylistItem> = Vec::new();

    for source in sources {
        let src_duration = source.image_duration;
        let mut path_items: Vec<PlaylistItem> = Vec::new();
        let mut url_items: Vec<PlaylistItem> = Vec::new();

        // ── Path-based scanning ────────────────────────────────────
        if let Some(ref path_str) = source.path {
            let root = Path::new(path_str);
            if root.is_dir() {
                scan_dir(root, source.recurse, 0, &mut path_items, src_duration);
            }
        }
        // Path items are sorted lexicographically for determinism
        // BEFORE any shuffle.
        path_items.sort_by(|a, b| a.uri.cmp(&b.uri));

        // ── URL-based items ────────────────────────────────────────
        // Kept in listed order — the operator authored the list
        // intentionally.
        if !source.urls.is_empty() {
            for url in &source.urls {
                url_items.push(PlaylistItem {
                    uri: url.clone(),
                    image_duration: src_duration,
                });
            }
        }

        // Combine: path items first, then URLs (config-order merge).
        let mut src_items: Vec<PlaylistItem> = Vec::new();
        src_items.append(&mut path_items);
        src_items.append(&mut url_items);

        // ── Shuffle (if requested) ─────────────────────────────────
        if source.shuffle && src_items.len() > 1 {
            // Fisher-Yates in-place with SplitMix64.
            #[allow(clippy::cast_possible_truncation)]
            for i in (1..src_items.len()).rev() {
                let j = (splitmix64_next(&mut rng) as usize) % (i + 1);
                src_items.swap(i, j);
            }
        }
        // `order` is silently ignored when `shuffle` wins; otherwise
        // "sequential" (the default) keeps the deterministic order.

        items.append(&mut src_items);
    }

    tracing::info!(
        event = "screensaver_playlist_built",
        source_count = sources.len(),
        item_count = items.len(),
    );

    items
}

// ── Internal helpers ───────────────────────────────────────────────────

/// Recursively scan `dir` for media files, appending [`PlaylistItem`]s
/// to `out`.  Symlinks are skipped (`is_symlink()` guard).
fn scan_dir(
    dir: &Path,
    recurse: bool,
    depth: u32,
    out: &mut Vec<PlaylistItem>,
    image_duration: Option<Duration>,
) {
    if depth > MAX_RECURSE_DEPTH {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        // permission denied, missing, etc. — skip silently
        return;
    };

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };

        // Never follow symlinks — prevents cycles and escapes.
        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            if recurse {
                scan_dir(&entry.path(), recurse, depth + 1, out, image_duration);
            }
            continue;
        }

        // Regular file — check extension.
        if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_ascii_lowercase();
            if IMAGE_EXTENSIONS.contains(&ext_lower.as_str())
                || VIDEO_EXTENSIONS.contains(&ext_lower.as_str())
            {
                // Use `to_string_lossy()` so non-UTF-8 paths are
                // included rather than silently dropped — mpv handles
                // arbitrary byte paths on Linux.
                out.push(PlaylistItem {
                    uri: entry.path().to_string_lossy().into_owned(),
                    image_duration,
                });
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a tempdir tree with the given structure.
    ///
    /// `tree` is a slice of relative paths within the tempdir.  Directories
    /// are created when a path ends with `/` or when a file lives under
    /// it.  Returns the tempdir (which is removed on drop).
    fn make_tree(entries: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for entry in entries {
            let full = dir.path().join(entry);
            if entry.ends_with('/') {
                fs::create_dir_all(&full).expect("mkdir");
            } else {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent).expect("mkdir parent");
                }
                fs::write(&full, b"").expect("write file");
            }
        }
        dir
    }

    /// Collect URIs from a playlist slice for comparison.
    fn uris(items: &[PlaylistItem]) -> Vec<&str> {
        items.iter().map(|i| i.uri.as_str()).collect()
    }

    // ── Scan tests ───────────────────────────────────────────────────

    #[test]
    fn scan_yields_exactly_media_set() {
        let tmp = make_tree(&[
            "a.jpg",
            "b.JPEG",
            "c.PNG",
            "d.webp",
            "e.bmp",
            "f.gif",
            "g.mp4",
            "h.MKV",
            "i.webm",
            "j.mov",
            "k.avi",
            "l.m4v",
            "readme.txt",
            "notes.md",
            ".hidden.jpg", // dotfiles ARE included (just another filename)
        ]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        // 12 media files + .hidden.jpg = 13
        assert_eq!(playlist.len(), 13);
        // Non-media files must NOT appear.
        for item in &playlist {
            assert!(
                !item.uri.ends_with("readme.txt") && !item.uri.ends_with("notes.md"),
                "non-media file leaked in: {}",
                item.uri
            );
        }
    }

    #[test]
    fn recurse_false_stays_top_level() {
        let tmp = make_tree(&[
            "top.jpg",
            "sub/",           // directory
            "sub/nested.png", // inside sub, should be skipped
        ]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        let u = uris(&playlist);
        assert_eq!(u.len(), 1, "only top.jpg should be found, got {u:?}");
        assert!(u[0].ends_with("top.jpg"), "unexpected item: {}", u[0]);
    }

    #[test]
    fn recurse_true_dives_into_nested_dirs() {
        let tmp = make_tree(&[
            "top.jpg",
            "sub/",
            "sub/nested.png",
            "sub/deep/",
            "sub/deep/bottom.mp4",
        ]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: true,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        let u = uris(&playlist);
        assert_eq!(u.len(), 3, "top.jpg + nested.png + bottom.mp4, got {u:?}");
        // Lexicographic sort: bottom.mp4 < nested.png < top.jpg (sort by full path)
        assert!(u[0].ends_with("bottom.mp4"));
        assert!(u[1].ends_with("nested.png"));
        assert!(u[2].ends_with("top.jpg"));
    }

    #[test]
    fn symlinks_are_skipped() {
        let tmp = make_tree(&["real.jpg", "sub/"]);
        // Create a symlink from sub/link.jpg → ../real.jpg
        let link = tmp.path().join("sub/link.jpg");
        let target = tmp.path().join("real.jpg");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: true,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        let u = uris(&playlist);
        // Only real.jpg — the symlink is skipped, preventing double-count
        // and cycle-escape.
        assert_eq!(u.len(), 1, "symlink must be skipped, got {u:?}");
        assert!(u[0].ends_with("real.jpg"));
    }

    #[test]
    fn case_insensitive_extensions() {
        let tmp = make_tree(&["img.JPG", "img.JpG", "vid.MP4", "vid.Mp4"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(playlist.len(), 4, "case variants all accepted");
    }

    #[test]
    fn deterministic_sort_pre_shuffle() {
        let tmp = make_tree(&["z.jpg", "a.png", "m.mp4", "c.webp"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        // No shuffle → strictly lexicographic.
        let playlist = build_playlist(&[source], Some(42));
        let u = uris(&playlist);
        assert_eq!(u.len(), 4);
        assert!(u[0].ends_with("a.png"));
        assert!(u[1].ends_with("c.webp"));
        assert!(u[2].ends_with("m.mp4"));
        assert!(u[3].ends_with("z.jpg"));
    }

    // ── Shuffle tests ────────────────────────────────────────────────

    #[test]
    fn seeded_shuffle_is_reproducible() {
        let tmp = make_tree(&[
            "a.jpg", "b.png", "c.mp4", "d.webp", "e.gif", "f.mkv", "g.jpeg", "h.avi",
        ]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: true,
            order: None,
            image_duration: None,
        };

        let p1 = build_playlist(std::slice::from_ref(&source), Some(0xDEAD_BEEF));
        let p2 = build_playlist(&[source], Some(0xDEAD_BEEF));

        assert_eq!(p1.len(), p2.len());
        for (a, b) in p1.iter().zip(p2.iter()) {
            assert_eq!(a.uri, b.uri, "same seed must produce same order");
        }
    }

    #[test]
    fn seeded_shuffle_permutes_order() {
        let tmp = make_tree(&[
            "a.jpg", "b.png", "c.mp4", "d.webp", "e.gif", "f.mkv", "g.jpeg", "h.avi",
        ]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: true,
            order: None,
            image_duration: None,
        };

        // Without shuffle, items are lexicographic.
        let no_shuffle = build_playlist(
            &[ScreensaverSource {
                shuffle: false,
                ..source.clone()
            }],
            Some(42),
        );
        let shuffled = build_playlist(&[source], Some(0xCAFE_BABE));
        let sorted_uris = uris(&no_shuffle);
        let shuffled_uris = uris(&shuffled);

        assert_eq!(sorted_uris.len(), shuffled_uris.len());
        assert_ne!(
            sorted_uris, shuffled_uris,
            "shuffle with seed 0xCAFE_BABE must permute the 8 items"
        );
    }

    #[test]
    fn shuffle_with_one_item_is_noop() {
        let tmp = make_tree(&["single.jpg"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: true,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(playlist.len(), 1);
        assert!(playlist[0].uri.ends_with("single.jpg"));
    }

    // ── URL tests ─────────────────────────────────────────────────────

    #[test]
    fn urls_keep_listed_order() {
        let source = ScreensaverSource {
            path: None,
            urls: vec![
                "https://example.com/z.jpg".into(),
                "https://example.com/a.mp4".into(),
                "https://example.com/m.png".into(),
            ],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: Some(Duration::from_secs(3)),
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(playlist.len(), 3);
        assert_eq!(playlist[0].uri, "https://example.com/z.jpg");
        assert_eq!(playlist[1].uri, "https://example.com/a.mp4");
        assert_eq!(playlist[2].uri, "https://example.com/m.png");
    }

    #[test]
    fn url_shuffle_honors_shuffle_flag() {
        let source = ScreensaverSource {
            path: None,
            urls: (1..=8).map(|i| format!("u{i}")).collect(),
            recurse: false,
            shuffle: true,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(0xDECAF));
        let u = uris(&playlist);
        assert_eq!(u.len(), 8);
        // With shuffle=true, order should differ from listed order.
        // 8 items + splitmix64 seed → identity permutation ≈ 1/40320.
        let listed: Vec<String> = (1..=8).map(|i| format!("u{i}")).collect();
        let listed_refs: Vec<&str> = listed.iter().map(String::as_str).collect();
        assert_ne!(u, listed_refs);
    }

    // ── Merge tests ───────────────────────────────────────────────────

    #[test]
    fn merge_order_is_config_order() {
        let tmp1 = make_tree(&["src1_a.jpg", "src1_b.png"]);
        let tmp2 = make_tree(&["src2_x.mp4", "src2_y.webp"]);

        let s1 = ScreensaverSource {
            path: Some(tmp1.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: Some(Duration::from_secs(1)),
        };
        let s2 = ScreensaverSource {
            path: Some(tmp2.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: Some(Duration::from_secs(2)),
        };

        let playlist = build_playlist(&[s1, s2], Some(42));
        assert_eq!(playlist.len(), 4);
        // Source 1 items first.
        assert!(playlist[0].uri.contains("src1_"));
        assert!(playlist[1].uri.contains("src1_"));
        assert!(playlist[2].uri.contains("src2_"));
        assert!(playlist[3].uri.contains("src2_"));
        // Per-source durations attach.
        assert_eq!(playlist[0].image_duration, Some(Duration::from_secs(1)));
        assert_eq!(playlist[2].image_duration, Some(Duration::from_secs(2)));
    }

    #[test]
    fn per_item_durations_attach_per_source() {
        let tmp = make_tree(&["img.jpg"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: Some(Duration::from_millis(1500)),
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(
            playlist[0].image_duration,
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn none_duration_propagates() {
        let tmp = make_tree(&["img.jpg"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(playlist[0].image_duration, None);
    }

    #[test]
    fn both_path_and_urls_combines() {
        let tmp = make_tree(&["path_img.jpg"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec!["https://example.com/url_vid.mp4".into()],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: Some(Duration::from_secs(5)),
        };

        let playlist = build_playlist(&[source], Some(42));
        assert_eq!(playlist.len(), 2, "path item + url item");
        // path item first, then urls.
        assert!(playlist[0].uri.contains("path_img"), "path item first");
        assert!(playlist[1].uri.contains("url_vid"), "url item second");
    }

    #[test]
    fn empty_sources_yields_empty_playlist() {
        let playlist = build_playlist(&[], Some(42));
        assert!(playlist.is_empty());
    }

    #[test]
    fn neither_path_nor_urls_yields_empty() {
        let source = ScreensaverSource {
            path: None,
            urls: vec![],
            recurse: false,
            shuffle: false,
            order: None,
            image_duration: None,
        };
        let playlist = build_playlist(&[source], Some(42));
        assert!(playlist.is_empty());
    }

    #[test]
    fn recurse_depth_bound() {
        // Create a chain of 12 nested dirs — only first 8 levels scanned.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut current = tmp.path().to_path_buf();
        for i in 0..12 {
            current = current.join(format!("d{i}"));
            fs::create_dir_all(&current).expect("mkdir");
            // Put a file at each level.
            fs::write(current.join("f.jpg"), b"").expect("write");
        }

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: true,
            shuffle: false,
            order: None,
            image_duration: None,
        };

        let playlist = build_playlist(&[source], Some(42));
        // The scan root (tmp/) is at depth 0 and contains only the d0/
        // directory (no file).  d0/f.jpg is at depth 1, d1/f.jpg at
        // depth 2, ..., d7/f.jpg at depth 8 (≤ MAX).  d8/f.jpg at
        // depth 9 (> MAX) is skipped.  So 8 files survive.
        assert_eq!(
            playlist.len(),
            8,
            "depth bound at 8 yields 8 files (depth 1-8 inclusive)"
        );
    }

    #[test]
    fn different_seeds_produce_different_shuffles() {
        let tmp = make_tree(&["a.jpg", "b.png", "c.mp4", "d.webp", "e.gif"]);

        let source = ScreensaverSource {
            path: Some(tmp.path().to_string_lossy().into_owned()),
            urls: vec![],
            recurse: false,
            shuffle: true,
            order: None,
            image_duration: None,
        };

        let p1 = build_playlist(std::slice::from_ref(&source), Some(1));
        let p2 = build_playlist(&[source], Some(2));

        let u1 = uris(&p1);
        let u2 = uris(&p2);
        assert_eq!(u1.len(), u2.len());
        assert_ne!(u1, u2, "different seeds must produce different order");
    }
}
