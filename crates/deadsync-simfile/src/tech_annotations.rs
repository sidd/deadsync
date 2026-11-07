//! Per-row step-tech annotations for gameplay coaching overlays.
//!
//! `timing::crossover_annotations` only keeps the crossover flag that the
//! Crossover Cues feature needs. This adapter runs the same `rssp` step-parity
//! pass but keeps the other per-row tech categories (footswitches,
//! sideswitches, jacks) and drops rows without any tech, so callers get a
//! compact list of beat positions instead of chart totals. `rssp` stays behind
//! this crate; callers only see the domain struct below.

use crate::timing::{foot_masks, rssp_timing_segments_from_deadsync};
use deadsync_rules::timing::TimingSegments;
use rssp::timing as rssp_timing;

/// One chart row that the step-parity solver flagged with at least one tech
/// category.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TechRowAnnotation {
    pub beat: f32,
    /// Occupancy bitmask of the foot-bearing columns on this row.
    pub column_mask: u8,
    /// Columns stepped by the left / right foot on this row.
    pub left_foot_mask: u8,
    pub right_foot_mask: u8,
    pub crossover: bool,
    pub full_crossover: bool,
    /// Up/down-arrow footswitch.
    pub footswitch: bool,
    /// Left/right-arrow footswitch.
    pub sideswitch: bool,
    pub jack: bool,
    /// More than one foot-bearing note on the row.
    pub bracket: bool,
}

impl TechRowAnnotation {
    /// `true` when the stepping foot is the right foot only, i.e. a crossover
    /// on this row moves the right foot across (to the left of) the left foot.
    #[inline]
    #[must_use]
    pub const fn right_foot_only(&self) -> bool {
        self.right_foot_mask != 0 && self.left_foot_mask == 0
    }

    /// `true` when the stepping foot is the left foot only.
    #[inline]
    #[must_use]
    pub const fn left_foot_only(&self) -> bool {
        self.left_foot_mask != 0 && self.right_foot_mask == 0
    }
}

/// Runs step parity over `rows` and returns only the rows with tech, in chart
/// order. Returns an empty list for unsupported lane counts.
#[must_use]
pub fn tech_row_annotations<const LANES: usize>(
    rows: &[[u8; LANES]],
    row_to_beat: &[f32],
    segments: &TimingSegments,
) -> Vec<TechRowAnnotation> {
    if rows.is_empty() || rows.len() != row_to_beat.len() {
        return Vec::new();
    }
    let rssp_segments = rssp_timing_segments_from_deadsync(segments);
    let timing = rssp_timing::timing_data_from_segments(0.0, 0.0, &rssp_segments);
    let Some(mut scratch) = rssp::step_parity::timing_rows_scratch::<LANES>() else {
        return Vec::new();
    };
    rssp::step_parity::annotate_timing_rows(rows, row_to_beat, &timing, &mut scratch)
        .into_iter()
        .filter_map(|annotation| {
            let tech = annotation.row_tech;
            let row = TechRowAnnotation {
                beat: annotation.beat,
                column_mask: annotation.column_mask,
                left_foot_mask: 0,
                right_foot_mask: 0,
                crossover: tech.crossovers > 0,
                full_crossover: tech.full_crossovers > 0,
                footswitch: tech.footswitches > 0,
                sideswitch: tech.sideswitches > 0,
                jack: tech.jacks > 0,
                bracket: annotation.foot_count() > 1,
            };
            if !(row.crossover || row.footswitch || row.sideswitch || row.jack) {
                return None;
            }
            let (left_foot_mask, right_foot_mask) = foot_masks(&annotation);
            Some(TechRowAnnotation {
                left_foot_mask,
                right_foot_mask,
                ..row
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tech_row_annotations;

    fn segments() -> deadsync_rules::timing::TimingSegments {
        deadsync_rules::timing::TimingSegments {
            bpms: vec![(0.0, 120.0)],
            ..deadsync_rules::timing::TimingSegments::default()
        }
    }

    fn beats(count: usize, step: f32) -> Vec<f32> {
        (0..count).map(|i| i as f32 * step).collect()
    }

    #[test]
    fn empty_and_mismatched_inputs_yield_nothing() {
        let rows: [[u8; 4]; 0] = [];
        assert!(tech_row_annotations(&rows, &[], &segments()).is_empty());
        let rows = [*b"1000", *b"0001"];
        assert!(tech_row_annotations(&rows, &[0.0], &segments()).is_empty());
    }

    #[test]
    fn plain_alternating_stream_has_no_tech_rows() {
        // Left/Right alternating feet: a plain stream without tech.
        let rows = [*b"1000", *b"0001", *b"1000", *b"0001", *b"1000", *b"0001"];
        let annotations = tech_row_annotations(&rows, &beats(rows.len(), 0.5), &segments());
        assert!(annotations.iter().all(|row| !row.crossover));
    }

    #[test]
    fn crossover_pattern_is_flagged_with_crossing_foot() {
        // L D R D L D R: the right foot crosses back over to Left on each L.
        let rows = [
            *b"1000", *b"0100", *b"0001", *b"0100", *b"1000", *b"0100", *b"0001", *b"0100",
            *b"1000", *b"0100", *b"0001",
        ];
        let annotations = tech_row_annotations(&rows, &beats(rows.len(), 0.25), &segments());
        let crossovers: Vec<_> = annotations.iter().filter(|row| row.crossover).collect();
        assert!(
            !crossovers.is_empty(),
            "expected crossovers in {annotations:?}"
        );
        for row in crossovers {
            assert_eq!(row.column_mask.count_ones(), 1);
            assert!(row.left_foot_only() ^ row.right_foot_only());
            assert_eq!(row.left_foot_mask | row.right_foot_mask, row.column_mask);
        }
    }

    #[test]
    fn single_panel_repeats_are_flagged_as_jacks() {
        // rssp only counts fast jacks (< 0.176 s apart): 16ths at 120 BPM.
        let rows = [*b"1000", *b"1000", *b"1000", *b"1000"];
        let annotations = tech_row_annotations(&rows, &beats(rows.len(), 0.25), &segments());
        assert!(annotations.iter().any(|row| row.jack), "{annotations:?}");
        assert!(
            annotations
                .windows(2)
                .all(|pair| pair[0].beat < pair[1].beat)
        );
    }
}
