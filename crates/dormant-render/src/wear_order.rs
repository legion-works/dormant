//! Deterministic wear-even ordering for screensaver playlist cycles.

use dormant_core::spatial_grid::{HeatGrid, LUMA_GRID_COLS, LUMA_GRID_ROWS, resample_area};
use thiserror::Error;

use crate::luma::LumaCatalog;
use crate::playlist::{PlaylistItem, PlaylistOrder};

const CELL_COUNT: f64 = 144.0;

/// Failure while calculating a wear-even cycle.
#[derive(Debug, Error, PartialEq)]
pub enum WearOrderError {
    /// The catalog did not contain a luma grid for an item.
    #[error("missing luma grid for playlist item {uri}")]
    MissingLuma {
        /// URI of the item with no catalog entry.
        uri: String,
    },
    /// The heat snapshot dimensions or values were invalid.
    #[error("invalid heat grid")]
    InvalidHeat,
    /// A calculated score was not finite.
    #[error("non-finite wear-even score for playlist item {uri}")]
    NonFiniteScore {
        /// URI of the item whose score was invalid.
        uri: String,
    },
    /// Another thread poisoned the luma catalog lock.
    #[error("luma catalog lock is poisoned")]
    CatalogPoisoned,
}

fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Return every candidate exactly once, preferring bright content on cold cells.
///
/// # Errors
///
/// Returns an error without producing a partial order when a luma grid, heat
/// snapshot, or calculated score is invalid.
pub fn wear_even_cycle(
    items: &[PlaylistItem],
    catalog: &LumaCatalog,
    heat: &HeatGrid,
    temperature: f64,
    seed: u64,
) -> Result<Vec<PlaylistItem>, WearOrderError> {
    if !temperature.is_finite() || !(0.0..=1.0).contains(&temperature) {
        return Err(WearOrderError::InvalidHeat);
    }
    let heat_cells = resample_area(
        &heat.cells,
        heat.rows,
        heat.cols,
        LUMA_GRID_ROWS,
        LUMA_GRID_COLS,
    )
    .ok_or(WearOrderError::InvalidHeat)?;
    let guard = catalog
        .read()
        .map_err(|_| WearOrderError::CatalogPoisoned)?;
    let mut rng = seed;
    let mut scored = Vec::with_capacity(items.len());
    for item in items {
        let luma = guard
            .get(&item.uri)
            .ok_or_else(|| WearOrderError::MissingLuma {
                uri: item.uri.clone(),
            })?;
        let dot = luma
            .cells
            .iter()
            .zip(&heat_cells)
            .map(|(item_luma, cell_heat)| f64::from(*item_luma) * f64::from(*cell_heat))
            .sum::<f64>();
        #[allow(
            clippy::cast_precision_loss,
            reason = "SplitMix64 output is normalized to a bounded jitter"
        )]
        let jitter = (splitmix64_next(&mut rng) as f64 / u64::MAX as f64 - 0.5) * temperature;
        let score = dot / CELL_COUNT + jitter;
        if !score.is_finite() {
            return Err(WearOrderError::NonFiniteScore {
                uri: item.uri.clone(),
            });
        }
        scored.push((score, item));
    }
    scored.sort_by(|(left_score, left), (right_score, right)| {
        left_score
            .total_cmp(right_score)
            .then_with(|| left.uri.cmp(&right.uri))
    });
    Ok(scored.into_iter().map(|(_, item)| item.clone()).collect())
}

/// Apply wear-even ordering to each contiguous wear-even source group.
///
/// Non-wear-even groups remain in their original positions. Any calculation
/// failure returns an error before exposing a partially reordered playlist.
///
/// # Errors
///
/// Returns the first wear-order calculation error without returning a partial
/// playlist.
pub fn apply_wear_even_groups(
    items: &[PlaylistItem],
    catalog: &LumaCatalog,
    heat: &HeatGrid,
    temperature: f64,
    seed: u64,
) -> Result<Vec<PlaylistItem>, WearOrderError> {
    let mut output = items.to_vec();
    let mut start = 0;
    while start < items.len() {
        let key = (items[start].source_index, items[start].order);
        let mut end = start + 1;
        while end < items.len() && (items[end].source_index, items[end].order) == key {
            end += 1;
        }
        if key.1 == PlaylistOrder::WearEven {
            let ordered = wear_even_cycle(&items[start..end], catalog, heat, temperature, seed)?;
            output[start..end].clone_from_slice(&ordered);
        }
        start = end;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use dormant_core::spatial_grid::{HeatGrid, LumaGrid};
    use proptest::prelude::*;

    use super::{WearOrderError, wear_even_cycle};
    use crate::playlist::{MediaKind, PlaylistItem, PlaylistOrder};

    fn item(uri: &str) -> PlaylistItem {
        PlaylistItem {
            uri: uri.to_owned(),
            image_duration: None,
            media_kind: MediaKind::Image,
            wear_tag: None,
            source_index: 0,
            order: PlaylistOrder::WearEven,
        }
    }

    fn catalog(items: &[PlaylistItem], value: f32) -> crate::luma::LumaCatalog {
        Arc::new(RwLock::new(
            items
                .iter()
                .map(|item| (item.uri.clone(), LumaGrid::new(vec![value; 144]).unwrap()))
                .collect::<HashMap<_, _>>(),
        ))
    }

    #[test]
    fn bright_left_item_wins_when_left_side_is_cold() {
        let items = vec![item("bright"), item("dark")];
        let bright = {
            let mut cells = vec![0.0; 144];
            for row in 0..9 {
                cells[row * 16..row * 16 + 8].fill(1.0);
            }
            LumaGrid::new(cells).unwrap()
        };
        let dark = LumaGrid::new(vec![0.1; 144]).unwrap();
        let catalog = Arc::new(RwLock::new(HashMap::from([
            ("bright".to_owned(), bright),
            ("dark".to_owned(), dark),
        ])));
        let heat = HeatGrid::new(1, 2, vec![0.0, 1.0]).unwrap();

        let ordered = wear_even_cycle(&items, &catalog, &heat, 0.0, 7).unwrap();

        assert_eq!(ordered[0].uri, "bright");
    }

    #[test]
    fn zero_temperature_and_seed_are_reproducible() {
        let items = vec![item("a"), item("b"), item("c")];
        let catalog = catalog(&items, 0.5);
        let heat = HeatGrid::new(3, 3, vec![0.0; 9]).unwrap();

        let first = wear_even_cycle(&items, &catalog, &heat, 0.0, 1).unwrap();
        let second = wear_even_cycle(&items, &catalog, &heat, 0.0, 99).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn same_seed_reproduces_temperature_jitter() {
        let items = vec![item("a"), item("b"), item("c")];
        let catalog = catalog(&items, 0.5);
        let heat = HeatGrid::new(3, 3, vec![0.5; 9]).unwrap();

        let first = wear_even_cycle(&items, &catalog, &heat, 1.0, 42).unwrap();
        let second = wear_even_cycle(&items, &catalog, &heat, 1.0, 42).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn missing_luma_returns_fallback_error_not_partial_order() {
        let items = vec![item("present"), item("missing")];
        let catalog = catalog(&items[..1], 0.5);
        let heat = HeatGrid::new(1, 1, vec![0.0]).unwrap();

        let result = wear_even_cycle(&items, &catalog, &heat, 0.0, 0);

        assert!(matches!(result, Err(WearOrderError::MissingLuma { .. })));
    }

    proptest! {
        #[test]
        fn every_cycle_is_a_permutation_with_no_starvation(
            values in prop::collection::vec(0.0f32..=1.0, 1..20),
            seed in any::<u64>(),
        ) {
            let items: Vec<_> = values.iter().enumerate().map(|(i, _)| item(&format!("{i}"))).collect();
            let catalog = catalog(&items, 0.5);
            let heat = HeatGrid::new(1, 1, vec![0.0]).unwrap();
            let ordered = wear_even_cycle(&items, &catalog, &heat, 0.05, seed).unwrap();
            let mut actual: Vec<_> = ordered.iter().map(|item| item.uri.as_str()).collect();
            let mut expected: Vec<_> = items.iter().map(|item| item.uri.as_str()).collect();
            actual.sort_unstable();
            expected.sort_unstable();
            prop_assert_eq!(actual, expected);
        }
    }
}
