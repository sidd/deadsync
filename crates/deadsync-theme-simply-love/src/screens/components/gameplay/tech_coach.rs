//! Tech Coach: a light gameplay guide that labels upcoming step tech
//! (crossovers, footswitches, sideswitches, fast jacks) next to the notefield
//! a moment before it arrives, so the player can focus on executing the tech
//! instead of reading it.
//!
//! Pipeline. Everything is computed once at screen entry; a frame only does
//! one binary search per display slot.
//! 1. `deadsync_simfile::tech_annotations` runs the step-parity solver (the
//!    same one behind Crossover Cues and the pattern info) and returns the
//!    chart rows that carry tech.
//! 2. [`events_from_annotations`] turns those rows into timed [`TechEvent`]s.
//! 3. [`schedule`] collapses dense tech into sustained runs and assigns each
//!    run a fixed display slot.
//! 4. [`TechCoachTrack::visible`] returns the (at most [`tuning::MAX_VISIBLE`])
//!    labels to draw at the current time, with their fade alpha and progress.
//!
//! Anti-clutter heuristics (constants in [`tuning`]):
//! * Runs. Events of one kind are folded into the current annotation whenever
//!   the next event's annotation would start before the current one has ended
//!   plus `MIN_GAP_S`. Same-kind tech closer than roughly
//!   `LEAD_S + HOLD_S + MIN_GAP_S` (about 2.2 s at 1.0x) therefore never
//!   re-flashes. The label stays up, shows a `×N` count, and a thin bar drains
//!   across the run until it ends.
//! * Timing. An annotation appears `LEAD_S` before its first step, stays on
//!   screen at least `MIN_DISPLAY_S`, and holds `HOLD_S` after its last step
//!   before fading out over `FADE_OUT_S`.
//! * Spacing and cap. Distinct annotations that share a slot are at least
//!   `MIN_GAP_S` apart, and there are only `MAX_VISIBLE` slots, so at most
//!   that many labels are ever visible. Runs are placed in priority order
//!   (crossover > footswitch > sideswitch > jack); a run that finds no free
//!   slot is dropped. It is never shown late, never stacked, and never takes
//!   over a slot.
//! * Noise. Kinds with `min_events > 1` (jacks) are only coached as runs, and
//!   bracket crossovers are skipped by default (like Crossover Cues, since
//!   they are often misidentified).
//! * Rate and scroll speed. Windows are in real (wall-clock) seconds and are
//!   scaled by the music rate. They do not depend on scroll speed: the lead is
//!   about the player's reaction time, not about how far away the arrows are.
//!
//! Only 4-panel styles (dance-single, and each side of versus) are coached.
//! Other styles get an empty track, which draws nothing.

use crate::act;
use crate::screens::gameplay::GameplayCoreState;
use deadlib_present::actors::Actor;
use deadsync_core::song_time::song_time_ns_to_seconds;
use deadsync_simfile::tech_annotations::{TechRowAnnotation, tech_row_annotations};
use std::fmt::Write as _;
use std::sync::Arc;

/// Every knob the owner may want to iterate on. Times are real-time seconds
/// at 1.0x and are scaled by the music rate in [`ScheduleParams::for_rate`].
pub(crate) mod tuning {
    /// How long before the first step of a run its annotation appears.
    pub const LEAD_S: f32 = 1.25;
    /// How long an annotation stays fully visible after the run's last step.
    pub const HOLD_S: f32 = 0.35;
    /// Minimum time an annotation stays up (from appearing to fade-out start),
    /// so a single quick event still reads as a calm label, not a blink.
    pub const MIN_DISPLAY_S: f32 = 1.5;
    /// Minimum blank time between two distinct annotations in the same slot.
    /// Anything closer is merged into one sustained annotation instead.
    pub const MIN_GAP_S: f32 = 0.6;
    pub const FADE_IN_S: f32 = 0.15;
    pub const FADE_OUT_S: f32 = 0.3;
    /// Maximum number of annotations on screen at once (display slots).
    pub const MAX_VISIBLE: usize = 2;
    /// Whether crossovers stepped as part of a bracket count as crossovers.
    pub const INCLUDE_BRACKET_CROSSOVERS: bool = false;
    /// Minimum events a jack run needs before it is coached. A single fast
    /// jack is easy to read; repeated jacks are where players lose the foot.
    pub const MIN_JACK_EVENTS: u16 = 2;
    /// Runs with at least this many events show a `×N` count.
    pub const SHOW_COUNT_FROM: u16 = 2;

    // Drawing. The first slot sits `COMBO_GAP_PX` past the combo, on the side
    // the arrows come from (below for standard scroll, above for reverse).
    pub const COMBO_GAP_PX: f32 = 46.0;
    pub const SLOT_STEP_PX: f32 = 20.0;
    pub const TEXT_ZOOM: f32 = 0.85;
    pub const MAX_ALPHA: f32 = 0.9;
    pub const BAR_WIDTH_PX: f32 = 64.0;
    pub const BAR_HEIGHT_PX: f32 = 2.0;
    pub const BAR_OFFSET_PX: f32 = 10.0;
    pub const BAR_ALPHA: f32 = 0.75;
}

// A slot's previous annotation must finish fading before the next one starts.
const _: () = assert!(tuning::MIN_GAP_S >= tuning::FADE_OUT_S);
const _: () = assert!(tuning::MAX_VISIBLE > 0);

const TECH_COACH_FONT: &str = "miso";
const TECH_COACH_Z: i16 = 84;

/// Tech categories in priority order (lower = placed first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TechKind {
    Crossover,
    Footswitch,
    Sideswitch,
    Jack,
}

impl TechKind {
    pub(crate) const ALL: [Self; 4] = [
        Self::Crossover,
        Self::Footswitch,
        Self::Sideswitch,
        Self::Jack,
    ];

    #[inline]
    const fn min_events(self) -> u16 {
        match self {
            Self::Jack => tuning::MIN_JACK_EVENTS,
            _ => 1,
        }
    }

    #[inline]
    const fn color(self) -> [f32; 3] {
        match self {
            Self::Crossover => [1.0, 0.72, 0.38],
            Self::Footswitch => [0.48, 0.86, 1.0],
            Self::Sideswitch => [0.80, 0.64, 1.0],
            Self::Jack => [1.0, 0.92, 0.48],
        }
    }

    #[inline]
    const fn label_key(self) -> &'static str {
        match self {
            Self::Crossover => "TechCoachCrossover",
            Self::Footswitch => "TechCoachFootswitch",
            Self::Sideswitch => "TechCoachSideswitch",
            Self::Jack => "TechCoachJacks",
        }
    }
}

/// The crossing foot moves toward the left of the pad (the right foot crosses
/// over the left foot).
pub(crate) const DIR_LEFTWARD: u8 = 1 << 0;
/// The crossing foot moves toward the right (the left foot crosses over).
pub(crate) const DIR_RIGHTWARD: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TechEvent {
    /// Song time of the step, in seconds.
    pub time: f32,
    pub kind: TechKind,
    /// `DIR_*` bits, crossovers only.
    pub dirs: u8,
}

/// Turns step-parity rows into timed events. One row can carry several kinds.
pub(crate) fn events_from_annotations(
    rows: &[TechRowAnnotation],
    mut beat_to_time: impl FnMut(f32) -> f32,
) -> Vec<TechEvent> {
    let mut events = Vec::new();
    for row in rows {
        let crossover = row.crossover && (tuning::INCLUDE_BRACKET_CROSSOVERS || !row.bracket);
        if !(crossover || row.footswitch || row.sideswitch || row.jack) {
            continue;
        }
        let time = beat_to_time(row.beat);
        if !time.is_finite() {
            continue;
        }
        if crossover {
            let dirs = if row.right_foot_only() {
                DIR_LEFTWARD
            } else if row.left_foot_only() {
                DIR_RIGHTWARD
            } else {
                0
            };
            events.push(TechEvent {
                time,
                kind: TechKind::Crossover,
                dirs,
            });
        }
        for (flag, kind) in [
            (row.footswitch, TechKind::Footswitch),
            (row.sideswitch, TechKind::Sideswitch),
            (row.jack, TechKind::Jack),
        ] {
            if flag {
                events.push(TechEvent {
                    time,
                    kind,
                    dirs: 0,
                });
            }
        }
    }
    events
}

/// Scheduling windows converted to song seconds for one music rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScheduleParams {
    pub lead: f32,
    pub hold: f32,
    pub min_display: f32,
    pub min_gap: f32,
    pub fade_in: f32,
    pub fade_out: f32,
}

impl ScheduleParams {
    /// Song time advances `rate` seconds per real second, so real-time windows
    /// are multiplied by the rate.
    pub(crate) fn for_rate(rate: f32) -> Self {
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            1.0
        };
        Self {
            lead: tuning::LEAD_S * rate,
            hold: tuning::HOLD_S * rate,
            min_display: tuning::MIN_DISPLAY_S * rate,
            min_gap: tuning::MIN_GAP_S * rate,
            fade_in: tuning::FADE_IN_S * rate,
            fade_out: tuning::FADE_OUT_S * rate,
        }
    }
}

/// One collapsed annotation: a run of same-kind tech events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TechRun {
    pub kind: TechKind,
    /// When the annotation appears (song seconds).
    pub start: f32,
    /// When the annotation starts fading out (song seconds).
    pub end: f32,
    pub first_event: f32,
    pub last_event: f32,
    pub count: u16,
    pub dirs: u8,
}

impl TechRun {
    fn new(event: TechEvent, params: ScheduleParams) -> Self {
        let start = event.time - params.lead;
        Self {
            kind: event.kind,
            start,
            end: (event.time + params.hold).max(start + params.min_display),
            first_event: event.time,
            last_event: event.time,
            count: 1,
            dirs: event.dirs,
        }
    }

    fn absorb(&mut self, event: TechEvent, params: ScheduleParams) {
        self.last_event = self.last_event.max(event.time);
        self.count = self.count.saturating_add(1);
        self.dirs |= event.dirs;
        self.end = (self.last_event + params.hold).max(self.start + params.min_display);
    }

    /// Fade alpha in `[0, 1]` at `now`; zero outside the visible window.
    pub(crate) fn alpha(&self, now: f32, params: ScheduleParams) -> f32 {
        if !(now >= self.start && now < self.end + params.fade_out) {
            return 0.0;
        }
        let fade_in = if params.fade_in > 0.0 {
            ((now - self.start) / params.fade_in).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let fade_out = if now <= self.end || params.fade_out <= 0.0 {
            1.0
        } else {
            (1.0 - (now - self.end) / params.fade_out).clamp(0.0, 1.0)
        };
        fade_in.min(fade_out)
    }

    /// Remaining fraction of a multi-event run: 1 until its first step, then
    /// draining to 0 at its last step. `None` for single events.
    pub(crate) fn remaining(&self, now: f32) -> Option<f32> {
        let span = self.last_event - self.first_event;
        (self.count >= 2 && span > 0.0).then(|| ((self.last_event - now) / span).clamp(0.0, 1.0))
    }
}

/// Folds one kind's events (sorted by time) into runs; see the module docs.
fn collapse_runs(events: &[TechEvent], kind: TechKind, params: ScheduleParams) -> Vec<TechRun> {
    let mut runs = Vec::new();
    let mut current: Option<TechRun> = None;
    for &event in events.iter().filter(|event| event.kind == kind) {
        if let Some(run) = current.as_mut() {
            if event.time - params.lead < run.end + params.min_gap {
                run.absorb(event, params);
                continue;
            }
            runs.push(*run);
        }
        current = Some(TechRun::new(event, params));
    }
    runs.extend(current);
    runs.retain(|run| run.count >= kind.min_events());
    runs
}

#[inline]
fn slot_is_free(slot: &[TechRun], run: &TechRun, min_gap: f32) -> bool {
    slot.iter()
        .all(|other| run.start >= other.end + min_gap || run.end + min_gap <= other.start)
}

/// Collapses events into runs and places them into at most
/// [`tuning::MAX_VISIBLE`] slots. Each slot's runs are sorted by start and
/// separated by at least `min_gap`.
pub(crate) fn schedule(
    events: &[TechEvent],
    params: ScheduleParams,
) -> [Vec<TechRun>; tuning::MAX_VISIBLE] {
    let mut sorted: Vec<TechEvent> = events
        .iter()
        .copied()
        .filter(|event| event.time.is_finite())
        .collect();
    sorted.sort_by(|a, b| a.time.total_cmp(&b.time));
    let mut slots: [Vec<TechRun>; tuning::MAX_VISIBLE] = std::array::from_fn(|_| Vec::new());
    for kind in TechKind::ALL {
        for run in collapse_runs(&sorted, kind, params) {
            if let Some(slot) = slots
                .iter_mut()
                .find(|slot| slot_is_free(slot, &run, params.min_gap))
            {
                let index = slot.partition_point(|other| other.start <= run.start);
                slot.insert(index, run);
            }
        }
    }
    slots
}

/// Label text for a run, e.g. `« CROSSOVER ×6`. The guillemets point the way
/// the crossing foot travels; a run that crosses both ways shows both.
pub(crate) fn label_text(base: &str, run: &TechRun) -> String {
    let mut text = String::with_capacity(base.len() + 12);
    let crossover = run.kind == TechKind::Crossover;
    if crossover && run.dirs & DIR_LEFTWARD != 0 {
        text.push_str("« ");
    }
    text.push_str(base);
    if crossover && run.dirs & DIR_RIGHTWARD != 0 {
        text.push_str(" »");
    }
    if run.count >= tuning::SHOW_COUNT_FROM {
        let _ = write!(text, " ×{}", run.count);
    }
    text
}

#[derive(Clone, Debug)]
pub(crate) struct TechCoachCue {
    pub run: TechRun,
    pub label: Arc<str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VisibleTechCue<'a> {
    pub slot: usize,
    pub cue: &'a TechCoachCue,
    pub alpha: f32,
    pub remaining: Option<f32>,
}

/// Song-lifetime, per-player Tech Coach annotations. Built at screen entry
/// and read-only afterwards. It is empty when the option is off or the style
/// is unsupported.
#[derive(Clone, Debug)]
pub(crate) struct TechCoachTrack {
    slots: [Vec<TechCoachCue>; tuning::MAX_VISIBLE],
    params: ScheduleParams,
}

impl Default for TechCoachTrack {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| Vec::new()),
            params: ScheduleParams::for_rate(1.0),
        }
    }
}

impl TechCoachTrack {
    pub(crate) fn from_runs(
        runs: [Vec<TechRun>; tuning::MAX_VISIBLE],
        params: ScheduleParams,
        mut base_label: impl FnMut(TechKind) -> Arc<str>,
    ) -> Self {
        let labels: [Arc<str>; 4] = TechKind::ALL.map(&mut base_label);
        let slots = runs.map(|slot| {
            slot.into_iter()
                .map(|run| TechCoachCue {
                    run,
                    label: Arc::from(label_text(&labels[run.kind as usize], &run)),
                })
                .collect()
        });
        Self { slots, params }
    }

    /// Builds the track for one runtime player, or an empty track when the
    /// option is off, the style is not 4-panel, or the chart has no tech.
    pub(crate) fn from_gameplay(state: &GameplayCoreState, player: usize, enabled: bool) -> Self {
        if !enabled || player >= state.num_players() || state.cols_per_player() != 4 {
            return Self::default();
        }
        let (Some(chart), Some(timing)) = (
            state.gameplay_chart(player),
            state.timing_for_player(player),
        ) else {
            return Self::default();
        };
        let note_range = state.note_range_for_player(player);
        if note_range.0 >= note_range.1 {
            return Self::default();
        }
        let col_start = player.saturating_mul(state.cols_per_player());
        let (rows, row_to_beat, _) =
            deadsync_gameplay::build_crossover_rows::<4>(state.notes(), note_range, col_start);
        let annotations = tech_row_annotations::<4>(&rows, &row_to_beat, &chart.timing_segments);
        let events = events_from_annotations(&annotations, |beat| {
            song_time_ns_to_seconds(timing.get_time_for_beat_ns(beat))
        });
        if events.is_empty() {
            return Self::default();
        }
        let params = ScheduleParams::for_rate(state.music_rate());
        Self::from_runs(schedule(&events, params), params, |kind| {
            crate::i18n::tr("Gameplay", kind.label_key())
        })
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(Vec::is_empty)
    }

    /// The annotations visible at song time `now` (one per slot at most).
    pub(crate) fn visible(&self, now: f32) -> impl Iterator<Item = VisibleTechCue<'_>> {
        let params = self.params;
        self.slots
            .iter()
            .enumerate()
            .filter_map(move |(slot, cues)| {
                let index = cues.partition_point(|cue| cue.run.start <= now);
                let cue = cues.get(index.checked_sub(1)?)?;
                let alpha = cue.run.alpha(now, params);
                (alpha > 0.0).then(|| VisibleTechCue {
                    slot,
                    cue,
                    alpha,
                    remaining: cue.run.remaining(now),
                })
            })
    }
}

pub(super) struct TechCoachFrame<'a> {
    pub track: &'a TechCoachTrack,
    /// Song time in seconds, as seen by this player's notefield.
    pub now: f32,
    pub center_x: f32,
    /// The combo's y; slots are laid out away from it toward incoming notes.
    pub combo_y: f32,
    pub reverse: bool,
}

/// Appends this frame's Tech Coach labels to the notefield HUD actors.
pub(super) fn compose(actors: &mut Vec<Actor>, frame: TechCoachFrame<'_>) {
    let toward_notes = if frame.reverse { -1.0 } else { 1.0 };
    for visible in frame.track.visible(frame.now) {
        let y = frame.combo_y
            + toward_notes * (tuning::COMBO_GAP_PX + visible.slot as f32 * tuning::SLOT_STEP_PX);
        let [r, g, b] = visible.cue.run.kind.color();
        let alpha = visible.alpha * tuning::MAX_ALPHA;
        actors.push(act!(text:
            font(TECH_COACH_FONT): settext(Arc::clone(&visible.cue.label)):
            align(0.5, 0.5): xy(frame.center_x, y):
            zoom(tuning::TEXT_ZOOM): horizalign(center):
            shadowcolor(0.0, 0.0, 0.0, alpha):
            shadowlength(1.0):
            diffuse(r, g, b, alpha):
            z(TECH_COACH_Z)
        ));
        if let Some(remaining) = visible.remaining
            && remaining > 0.0
        {
            actors.push(act!(quad:
                align(0.5, 0.5):
                xy(frame.center_x, y + tuning::BAR_OFFSET_PX):
                setsize(tuning::BAR_WIDTH_PX * remaining, tuning::BAR_HEIGHT_PX):
                diffuse(r, g, b, alpha * tuning::BAR_ALPHA):
                z(TECH_COACH_Z)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ScheduleParams {
        ScheduleParams::for_rate(1.0)
    }

    fn event(time: f32, kind: TechKind) -> TechEvent {
        TechEvent {
            time,
            kind,
            dirs: 0,
        }
    }

    fn runs_of(slots: &[Vec<TechRun>], kind: TechKind) -> Vec<TechRun> {
        slots
            .iter()
            .flatten()
            .copied()
            .filter(|run| run.kind == kind)
            .collect()
    }

    fn track(events: &[TechEvent]) -> TechCoachTrack {
        TechCoachTrack::from_runs(schedule(events, params()), params(), |_| Arc::from("X"))
    }

    #[test]
    fn single_event_gets_lead_minimum_display_and_hold() {
        let p = params();
        let slots = schedule(&[event(10.0, TechKind::Crossover)], p);
        assert_eq!(slots[0].len(), 1);
        assert!(slots[1..].iter().all(Vec::is_empty));
        let run = slots[0][0];
        assert_eq!(run.count, 1);
        assert!((run.start - (10.0 - tuning::LEAD_S)).abs() < 1e-5);
        let expected_end = (10.0 + tuning::HOLD_S).max(run.start + tuning::MIN_DISPLAY_S);
        assert!((run.end - expected_end).abs() < 1e-5);
        assert!(run.end - run.start >= tuning::MIN_DISPLAY_S - 1e-5);
    }

    #[test]
    fn crossover_spam_collapses_into_one_sustained_annotation() {
        // 24 crossovers, one every 8th note at 150 BPM (0.2 s apart).
        let events: Vec<_> = (0..24)
            .map(|i| event(30.0 + i as f32 * 0.2, TechKind::Crossover))
            .collect();
        let slots = schedule(&events, params());
        let runs = runs_of(&slots, TechKind::Crossover);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].count, 24);
        assert!((runs[0].first_event - 30.0).abs() < 1e-5);
        assert!((runs[0].last_event - (30.0 + 23.0 * 0.2)).abs() < 1e-4);
        assert!(runs[0].end >= runs[0].last_event + tuning::HOLD_S - 1e-5);
    }

    #[test]
    fn crossovers_a_measure_apart_do_not_reflash() {
        // One crossover per measure at 140 BPM (~1.71 s): one long annotation.
        let events: Vec<_> = (0..16)
            .map(|i| event(5.0 + i as f32 * (240.0 / 140.0), TechKind::Crossover))
            .collect();
        let runs = runs_of(&schedule(&events, params()), TechKind::Crossover);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].count, 16);
    }

    #[test]
    fn sparse_events_stay_separate_with_a_minimum_gap() {
        let events: Vec<_> = [5.0, 15.0, 25.0]
            .into_iter()
            .map(|time| event(time, TechKind::Footswitch))
            .collect();
        let slots = schedule(&events, params());
        let runs = runs_of(&slots, TechKind::Footswitch);
        assert_eq!(runs.len(), 3);
        for pair in runs.windows(2) {
            assert!(pair[1].start >= pair[0].end + tuning::MIN_GAP_S - 1e-5);
        }
    }

    #[test]
    fn events_just_inside_the_gap_merge_and_just_outside_split() {
        let p = params();
        let first = TechRun::new(event(10.0, TechKind::Crossover), p);
        // The latest second event that still merges starts exactly at
        // `end + min_gap - epsilon` once its lead is applied.
        let merge_time = first.end + p.min_gap + p.lead - 0.01;
        let split_time = first.end + p.min_gap + p.lead + 0.01;
        let merged = schedule(
            &[
                event(10.0, TechKind::Crossover),
                event(merge_time, TechKind::Crossover),
            ],
            p,
        );
        assert_eq!(runs_of(&merged, TechKind::Crossover).len(), 1);
        let split = schedule(
            &[
                event(10.0, TechKind::Crossover),
                event(split_time, TechKind::Crossover),
            ],
            p,
        );
        assert_eq!(runs_of(&split, TechKind::Crossover).len(), 2);
    }

    #[test]
    fn visible_annotations_are_capped_and_prioritized() {
        // Three kinds overlapping at once: crossover and footswitch win the
        // two slots, the sideswitch run is dropped rather than stacked.
        let events = [
            event(20.0, TechKind::Sideswitch),
            event(20.0, TechKind::Footswitch),
            event(20.0, TechKind::Crossover),
        ];
        let slots = schedule(&events, params());
        assert_eq!(slots[0].len(), 1);
        assert_eq!(slots[0][0].kind, TechKind::Crossover);
        assert_eq!(slots[1].len(), 1);
        assert_eq!(slots[1][0].kind, TechKind::Footswitch);
        assert!(runs_of(&slots, TechKind::Sideswitch).is_empty());
    }

    #[test]
    fn lower_priority_runs_use_the_free_slot_between_crossovers() {
        let events = [
            event(10.0, TechKind::Crossover),
            event(30.0, TechKind::Crossover),
            event(20.0, TechKind::Jack),
            event(20.1, TechKind::Jack),
        ];
        let slots = schedule(&events, params());
        assert_eq!(slots[0].len(), 3);
        assert_eq!(slots[0][1].kind, TechKind::Jack);
        assert!(slots[1].is_empty());
    }

    #[test]
    fn a_single_jack_is_not_coached_but_a_jack_run_is() {
        let single = schedule(&[event(10.0, TechKind::Jack)], params());
        assert!(single.iter().all(Vec::is_empty));
        let pair = schedule(
            &[event(10.0, TechKind::Jack), event(10.3, TechKind::Jack)],
            params(),
        );
        assert_eq!(runs_of(&pair, TechKind::Jack).len(), 1);
    }

    #[test]
    fn music_rate_scales_windows_into_song_time() {
        let fast = ScheduleParams::for_rate(2.0);
        assert!((fast.lead - 2.0 * tuning::LEAD_S).abs() < 1e-6);
        assert!((fast.min_gap - 2.0 * tuning::MIN_GAP_S).abs() < 1e-6);
        assert_eq!(ScheduleParams::for_rate(0.0), ScheduleParams::for_rate(1.0));
        assert_eq!(
            ScheduleParams::for_rate(f32::NAN),
            ScheduleParams::for_rate(1.0)
        );
        let run = schedule(&[event(10.0, TechKind::Crossover)], fast)[0][0];
        assert!((run.start - (10.0 - 2.0 * tuning::LEAD_S)).abs() < 1e-5);
    }

    #[test]
    fn alpha_fades_in_holds_and_fades_out_once() {
        let p = params();
        let run = TechRun::new(event(10.0, TechKind::Crossover), p);
        assert_eq!(run.alpha(run.start - 0.01, p), 0.0);
        assert!(run.alpha(run.start + p.fade_in * 0.5, p) < 1.0);
        assert_eq!(run.alpha(run.start + p.fade_in + 0.01, p), 1.0);
        assert_eq!(run.alpha(run.end, p), 1.0);
        assert!(run.alpha(run.end + p.fade_out * 0.5, p) < 1.0);
        assert_eq!(run.alpha(run.end + p.fade_out, p), 0.0);
    }

    #[test]
    fn remaining_drains_across_the_run() {
        let p = params();
        let mut run = TechRun::new(event(10.0, TechKind::Crossover), p);
        assert_eq!(run.remaining(9.0), None);
        run.absorb(event(14.0, TechKind::Crossover), p);
        assert_eq!(run.remaining(9.0), Some(1.0));
        assert!((run.remaining(12.0).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(run.remaining(15.0), Some(0.0));
    }

    #[test]
    fn dense_mixed_tech_never_strobes_or_exceeds_the_cap() {
        // A pseudo-random dense chart: tech roughly every 0.1-0.6 s for a
        // minute, all kinds mixed.
        let mut seed = 0x2545_f491_u32;
        let mut time = 0.0;
        let mut events = Vec::new();
        while time < 60.0 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            time += 0.1 + (seed % 50) as f32 / 100.0;
            events.push(event(time, TechKind::ALL[(seed >> 8) as usize % 4]));
        }
        let track = track(&events);
        let mut was_visible = [false; tuning::MAX_VISIBLE];
        let mut appearances = [0usize; tuning::MAX_VISIBLE];
        let mut step = 0;
        while step < 7500 {
            let now = -5.0 + step as f32 * 0.01;
            let visible: Vec<_> = track.visible(now).collect();
            assert!(visible.len() <= tuning::MAX_VISIBLE);
            for slot in 0..tuning::MAX_VISIBLE {
                let is_visible = visible.iter().any(|cue| cue.slot == slot);
                if is_visible && !was_visible[slot] {
                    appearances[slot] += 1;
                }
                was_visible[slot] = is_visible;
            }
            step += 1;
        }
        for (slot, cues) in track.slots.iter().enumerate() {
            // Every annotation appears exactly once (no re-flash) ...
            assert_eq!(appearances[slot], cues.len());
            // ... and distinct annotations in a slot keep the minimum gap.
            for pair in cues.windows(2) {
                assert!(pair[1].run.start >= pair[0].run.end + tuning::MIN_GAP_S - 1e-4);
            }
        }
        // The dense stretch collapses: far fewer annotations than events.
        let total: usize = track.slots.iter().map(Vec::len).sum();
        assert!(
            total * 4 < events.len(),
            "{total} cues for {} events",
            events.len()
        );
    }

    #[test]
    fn labels_show_direction_and_count() {
        let p = params();
        let mut run = TechRun::new(
            TechEvent {
                time: 1.0,
                kind: TechKind::Crossover,
                dirs: DIR_LEFTWARD,
            },
            p,
        );
        assert_eq!(label_text("CROSSOVER", &run), "« CROSSOVER");
        run.absorb(
            TechEvent {
                time: 1.5,
                kind: TechKind::Crossover,
                dirs: DIR_RIGHTWARD,
            },
            p,
        );
        assert_eq!(label_text("CROSSOVER", &run), "« CROSSOVER » ×2");
        let jacks = TechRun {
            kind: TechKind::Jack,
            count: 3,
            dirs: DIR_LEFTWARD,
            ..run
        };
        assert_eq!(label_text("JACKS", &jacks), "JACKS ×3");
    }

    #[test]
    fn annotation_rows_map_to_events() {
        let rows = [
            TechRowAnnotation {
                beat: 1.0,
                column_mask: 0b0001,
                right_foot_mask: 0b0001,
                crossover: true,
                ..TechRowAnnotation::default()
            },
            TechRowAnnotation {
                beat: 2.0,
                column_mask: 0b1000,
                left_foot_mask: 0b1000,
                crossover: true,
                footswitch: true,
                ..TechRowAnnotation::default()
            },
            // Bracket crossovers are skipped by default.
            TechRowAnnotation {
                beat: 3.0,
                column_mask: 0b0011,
                right_foot_mask: 0b0011,
                crossover: true,
                bracket: true,
                ..TechRowAnnotation::default()
            },
        ];
        let events = events_from_annotations(&rows, |beat| beat * 0.5);
        assert_eq!(
            events,
            vec![
                TechEvent {
                    time: 0.5,
                    kind: TechKind::Crossover,
                    dirs: DIR_LEFTWARD,
                },
                TechEvent {
                    time: 1.0,
                    kind: TechKind::Crossover,
                    dirs: DIR_RIGHTWARD,
                },
                TechEvent {
                    time: 1.0,
                    kind: TechKind::Footswitch,
                    dirs: 0,
                },
            ]
        );
        assert!(events_from_annotations(&rows, |_| f32::NAN).is_empty());
    }

    #[test]
    fn empty_input_and_unsorted_input_are_safe() {
        let empty = track(&[]);
        assert!(empty.is_empty());
        assert_eq!(empty.visible(0.0).count(), 0);
        let slots = schedule(
            &[
                event(12.0, TechKind::Crossover),
                event(f32::NAN, TechKind::Crossover),
                event(11.0, TechKind::Crossover),
            ],
            params(),
        );
        let runs = runs_of(&slots, TechKind::Crossover);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].count, 2);
        assert!((runs[0].first_event - 11.0).abs() < 1e-6);
    }
}
