//! sidd fork: show whether the external recorder is recording, under the
//! stats overlay.
//!
//! The recorder rewrites `<data dir>/save/play-events/recorder-status.json`
//! about once a second. A background thread reads it and builds the text; the
//! game thread only picks up the latest result. Without the play-events
//! directory nothing is shown.

use crate::latest_worker_value::LatestWorkerValue;
use deadlib_present::actors::Actor;
use deadsync_theme_simply_love::screens::components::shared::recorder_status::{self, Line, Tone};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FILE: &str = "recorder-status.json";
const SCHEMA: u32 = 1;
const POLL: Duration = Duration::from_millis(500);
/// The recorder is not running once its file is older than this.
const STALE_MS: u64 = 5_000;
/// Time a new play is given to start recording before it is flagged.
const START_GRACE: Duration = Duration::from_secs(3);
const MAX_PROBLEMS: usize = 3;
const MAX_PROBLEM_CHARS: usize = 40;
/// The reader lives as long as its slot, so there is only one generation.
const SLOT: u64 = 1;

/// The recorder asked for pad sensor values (`pad_pressure`).
static PAD_PRESSURE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
}

/// Start following the status file in `dir`. Call once, on the game thread.
pub(crate) fn init(dir: PathBuf) {
    MONITOR.with_borrow_mut(|monitor| *monitor = Some(Monitor::start(dir)));
}

/// Whether a fresh status file asks for pad pressure. Never blocks.
pub(crate) fn pad_pressure_requested() -> bool {
    PAD_PRESSURE.load(Ordering::Relaxed)
}

/// Append the recorder lines under the stats overlay. Call right after
/// `stats_overlay::push`; `stats_start` is `actors.len()` from before it.
pub(crate) fn push(actors: &mut Vec<Actor>, stats_start: usize, in_gameplay: bool) {
    let overdue = overdue(in_gameplay, crate::play_events::play_age());
    MONITOR.with_borrow_mut(|monitor| {
        if let Some(monitor) = monitor {
            recorder_status::push(actors, stats_start, monitor.lines(overdue));
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Recording,
    Finishing,
    Idle,
    Disabled,
    Error,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
struct StatusFile {
    schema: u32,
    updated_unix_ms: i64,
    state: State,
    seconds: Option<f64>,
    dropped_frames: Option<u64>,
    problems: Option<Vec<String>>,
    pad_pressure: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    /// No play-events directory: the recorder integration is not set up.
    Hidden,
    /// Missing, unreadable or stale status file.
    Offline,
    Fresh(StatusFile),
}

fn read_status(dir: &Path, now_ms: i64) -> Status {
    let file = std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<StatusFile>(&bytes).ok());
    match file {
        Some(file)
            if file.schema == SCHEMA && now_ms.abs_diff(file.updated_unix_ms) <= STALE_MS =>
        {
            Status::Fresh(file)
        }
        _ if dir.is_dir() => Status::Offline,
        _ => Status::Hidden,
    }
}

fn wants_pad_pressure(status: &Status) -> bool {
    matches!(status, Status::Fresh(file) if file.pad_pressure == Some(true))
}

fn line(text: impl Into<Arc<str>>, tone: Tone) -> Line {
    Line {
        text: text.into(),
        tone,
    }
}

/// `overdue` means a play has been on screen long enough that it should be
/// recording by now.
fn select_lines(status: &Status, overdue: bool) -> Vec<Line> {
    let file = match status {
        Status::Hidden => return Vec::new(),
        Status::Offline => None,
        Status::Fresh(file) => Some(file),
    };
    let state = match file.map(|file| file.state) {
        Some(State::Disabled) => line("REC OFF", Tone::Normal),
        Some(State::Recording) => {
            // `as` saturates, so a negative duration reads as zero.
            let seconds = file.and_then(|file| file.seconds).unwrap_or(0.0) as u64;
            line(
                format!("REC {}:{:02}", seconds / 60, seconds % 60),
                Tone::Alert,
            )
        }
        _ if overdue => line("REC NOT RECORDING", Tone::Alert),
        Some(State::Finishing) => line("REC SAVING", Tone::Normal),
        Some(State::Idle) => line("REC READY", Tone::Normal),
        Some(State::Error) => line("REC ERROR", Tone::Alert),
        None => line("REC OFFLINE", Tone::Warning),
    };
    let mut lines = vec![state];
    if let Some(file) = file {
        for problem in file.problems.iter().flatten().take(MAX_PROBLEMS) {
            lines.push(line(short(problem), Tone::Warning));
        }
        if let Some(dropped @ 1..) = file.dropped_frames {
            lines.push(line(format!("REC DROPPED {dropped}"), Tone::Warning));
        }
    }
    lines
}

/// `problem` as a single line of at most `MAX_PROBLEM_CHARS` characters.
fn short(problem: &str) -> String {
    let mut text: String = problem
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().count() > MAX_PROBLEM_CHARS {
        text = text.chars().take(MAX_PROBLEM_CHARS - 3).collect();
        text.push_str("...");
    }
    text
}

fn overdue(in_gameplay: bool, play_age: Option<Duration>) -> bool {
    in_gameplay && play_age.is_some_and(|age| age > START_GRACE)
}

/// Both texts are built by the reader, so choosing between them is free.
#[derive(Clone, Debug, Default, PartialEq)]
struct Panel {
    normal: Vec<Line>,
    overdue: Vec<Line>,
}

impl Panel {
    fn new(status: &Status) -> Self {
        Self {
            normal: select_lines(status, false),
            overdue: select_lines(status, true),
        }
    }
}

struct Monitor {
    latest: Arc<LatestWorkerValue<Panel>>,
    shown: Panel,
}

impl Monitor {
    fn start(dir: PathBuf) -> Self {
        let latest = Arc::new(LatestWorkerValue::default());
        latest.start(SLOT);
        let slot = Arc::downgrade(&latest);
        if let Err(error) = std::thread::Builder::new()
            .name("recorder-status".into())
            .spawn(move || follow(&dir, &slot))
        {
            log::warn!("Failed to start the recorder status reader: {error}");
        }
        Self {
            latest,
            shown: Panel::default(),
        }
    }

    /// The newest lines. Never blocks and never allocates.
    fn lines(&mut self, overdue: bool) -> &[Line] {
        if let Some(panel) = self.latest.take(SLOT) {
            self.shown = panel;
        }
        if overdue {
            &self.shown.overdue
        } else {
            &self.shown.normal
        }
    }
}

/// Reader thread. Publishes only changes and exits when the monitor is gone.
fn follow(dir: &Path, slot: &Weak<LatestWorkerValue<Panel>>) {
    let mut published = Panel::default();
    loop {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            });
        let status = read_status(dir, now_ms);
        PAD_PRESSURE.store(wants_pad_pressure(&status), Ordering::Relaxed);
        let panel = Panel::new(&status);
        let Some(latest) = slot.upgrade() else {
            return;
        };
        if panel != published {
            published.clone_from(&panel);
            latest.publish(SLOT, panel);
        }
        drop(latest);
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadlib_render_core::{BackendType, ClockDomainTrace, PresentModeTrace};
    use deadsync_theme_simply_love::screens::components::shared::{recorder_status, stats_overlay};
    use deadsync_theme_simply_love::views::TimingHealth;
    use std::fs;
    use std::time::Instant;

    const NOW: i64 = 1_795_677_000_000;

    fn status_json(state: &str, extra: &str) -> String {
        format!(r#"{{"schema":1,"updated_unix_ms":{NOW},"state":"{state}"{extra}}}"#)
    }

    fn read(contents: &str, now_ms: i64) -> Status {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("recorder-status.json"), contents).unwrap();
        read_status(dir.path(), now_ms)
    }

    fn shown(status: &Status, overdue: bool) -> Vec<(String, Tone)> {
        select_lines(status, overdue)
            .iter()
            .map(|line| (line.text.to_string(), line.tone))
            .collect()
    }

    fn line(text: &str, tone: Tone) -> (String, Tone) {
        (text.to_owned(), tone)
    }

    #[test]
    fn nothing_is_shown_without_the_play_events_directory() {
        let root = tempfile::tempdir().unwrap();
        let status = read_status(&root.path().join("play-events"), NOW);
        assert_eq!(status, Status::Hidden);
        assert!(shown(&status, false).is_empty());
        assert!(shown(&status, true).is_empty());
    }

    #[test]
    fn a_missing_or_unreadable_file_is_offline() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_status(dir.path(), NOW), Status::Offline);
        fs::create_dir(dir.path().join("recorder-status.json")).unwrap();
        assert_eq!(read_status(dir.path(), NOW), Status::Offline);
        for contents in [
            "",
            "{\"schema\":1,",
            r#"{"schema":1,"state":"idle"}"#,
            &status_json("warming-up", ""),
            &status_json("idle", "").replace("\"schema\":1", "\"schema\":2"),
        ] {
            assert_eq!(read(contents, NOW), Status::Offline, "{contents}");
        }
    }

    #[test]
    fn the_documented_example_is_read() {
        let example = r#"{"schema":1,"updated_unix_ms":1795677000000,"state":"recording",
            "seconds":83.2,"dropped_frames":0,"problems":[],"pending":0,
            "disk_free_gb":612.4,"last_saved":null}"#;
        assert_eq!(
            shown(&read(example, NOW + 900), false),
            [line("REC 1:23", Tone::Alert)]
        );
    }

    #[test]
    fn pad_pressure_is_wanted_only_when_a_fresh_file_asks() {
        assert!(wants_pad_pressure(&read(
            &status_json("idle", r#","pad_pressure":true"#),
            NOW
        )));
        for (extra, now) in [
            (r#","pad_pressure":true"#, NOW + 5_001),
            (r#","pad_pressure":false"#, NOW),
            ("", NOW),
        ] {
            assert!(
                !wants_pad_pressure(&read(&status_json("idle", extra), now)),
                "{extra}"
            );
        }
        assert!(!wants_pad_pressure(&Status::Hidden));
    }

    #[test]
    fn a_file_older_than_five_seconds_is_offline() {
        let contents = status_json("recording", "");
        assert!(matches!(read(&contents, NOW + 5_000), Status::Fresh(_)));
        assert_eq!(read(&contents, NOW + 5_001), Status::Offline);
        // A timestamp far ahead of the clock is not being refreshed either.
        assert!(matches!(read(&contents, NOW - 5_000), Status::Fresh(_)));
        assert_eq!(read(&contents, NOW - 5_001), Status::Offline);
    }

    #[test]
    fn optional_fields_may_be_null_or_absent() {
        for extra in [
            "",
            r#","seconds":null,"dropped_frames":null,"problems":null"#,
        ] {
            assert_eq!(
                shown(&read(&status_json("recording", extra), NOW), false),
                [line("REC 0:00", Tone::Alert)]
            );
        }
    }

    #[test]
    fn each_state_has_its_line() {
        for (state, expected) in [
            ("finishing", line("REC SAVING", Tone::Normal)),
            ("idle", line("REC READY", Tone::Normal)),
            ("disabled", line("REC OFF", Tone::Normal)),
            ("error", line("REC ERROR", Tone::Alert)),
        ] {
            let status = read(&status_json(state, ""), NOW);
            assert_eq!(shown(&status, false), [expected], "{state}");
        }
        assert_eq!(
            shown(&Status::Offline, false),
            [line("REC OFFLINE", Tone::Warning)]
        );
    }

    #[test]
    fn recording_time_is_minutes_and_seconds() {
        for (seconds, expected) in [
            ("0", "REC 0:00"),
            ("9.99", "REC 0:09"),
            ("60", "REC 1:00"),
            ("3725.5", "REC 62:05"),
            ("-3", "REC 0:00"),
        ] {
            let extra = format!(r#","seconds":{seconds}"#);
            let status = read(&status_json("recording", &extra), NOW);
            assert_eq!(shown(&status, false)[0].0, expected);
        }
    }

    #[test]
    fn an_overdue_play_that_is_not_recording_is_flagged() {
        let alert = line("REC NOT RECORDING", Tone::Alert);
        for state in ["finishing", "idle", "error"] {
            let status = read(&status_json(state, ""), NOW);
            assert_eq!(shown(&status, true), [alert.clone()], "{state}");
        }
        assert_eq!(shown(&Status::Offline, true), [alert]);
        assert_eq!(
            shown(&read(&status_json("disabled", ""), NOW), true),
            [line("REC OFF", Tone::Normal)]
        );
        assert_eq!(
            shown(&read(&status_json("recording", ""), NOW), true),
            [line("REC 0:00", Tone::Alert)]
        );
    }

    #[test]
    fn a_play_is_overdue_after_three_seconds_on_gameplay() {
        let three = Duration::from_secs(3);
        let longer = three + Duration::from_millis(1);
        assert!(overdue(true, Some(longer)));
        assert!(!overdue(true, Some(three)));
        // Practice and menus are not the Gameplay screen.
        assert!(!overdue(false, Some(longer)));
        assert!(!overdue(true, None));
    }

    #[test]
    fn dropped_frames_and_problems_get_their_own_warning_lines() {
        let extra =
            r#","dropped_frames":12,"problems":["camera stalled","disk space low","c","d"]"#;
        let status = read(&status_json("idle", extra), NOW);
        let details = [
            line("camera stalled", Tone::Warning),
            line("disk space low", Tone::Warning),
            line("c", Tone::Warning),
            line("REC DROPPED 12", Tone::Warning),
        ];
        let mut expected = vec![line("REC READY", Tone::Normal)];
        expected.extend(details.clone());
        assert_eq!(shown(&status, false), expected);
        expected[0] = line("REC NOT RECORDING", Tone::Alert);
        assert_eq!(shown(&status, true), expected);
    }

    #[test]
    fn problems_stay_on_one_short_line() {
        let long = "x".repeat(41);
        let fits = "é".repeat(40);
        let extra = format!(r#","problems":["{long}","{fits}","two\nlines"]"#);
        let status = read(&status_json("idle", &extra), NOW);
        let lines = shown(&status, false);
        assert_eq!(lines[1].0, format!("{}...", "x".repeat(37)));
        assert_eq!(lines[2].0, fits);
        assert_eq!(lines[3].0, "two lines");
    }

    fn wait_for(monitor: &mut Monitor, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while monitor.lines(false).first().map(|line| &*line.text) != Some(expected) {
            assert!(Instant::now() < deadline, "never showed {expected}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_background_reader_follows_the_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("play-events");
        let mut monitor = Monitor::start(dir.clone());
        assert!(monitor.lines(false).is_empty());
        fs::create_dir(&dir).unwrap();
        wait_for(&mut monitor, "REC OFFLINE");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let contents = status_json("idle", "").replace(&NOW.to_string(), &now_ms.to_string());
        fs::write(dir.join("recorder-status.json"), contents).unwrap();
        wait_for(&mut monitor, "REC READY");
        assert_eq!(&*monitor.lines(true)[0].text, "REC NOT RECORDING");
        // An unchanged file keeps handing out the same text allocation.
        let before = std::sync::Arc::clone(&monitor.lines(false)[0].text);
        std::thread::sleep(Duration::from_millis(700));
        assert!(std::sync::Arc::ptr_eq(
            &before,
            &monitor.lines(false)[0].text
        ));
    }

    fn timing() -> TimingHealth {
        TimingHealth {
            interval_ns: 0,
            display_error_ms: 0.0,
            display_catching_up: false,
            present_mode: PresentModeTrace::Fifo,
            display_clock: ClockDomainTrace::Device,
            host_clock: ClockDomainTrace::Monotonic,
            in_flight_images: 0,
            waited_for_image: false,
            applied_back_pressure: false,
            queue_idle_waited: false,
            suboptimal: false,
            submitted_present_id: 0,
            completed_present_id: 0,
            calibration_error_ns: 0,
            host_mapped: false,
            audio: None,
        }
    }

    /// `(text, y)` for every text actor.
    fn rows(actors: &[Actor]) -> Vec<(String, f32)> {
        actors
            .iter()
            .filter_map(|actor| match actor {
                Actor::Text {
                    content, offset, ..
                } => Some((content.as_str().to_owned(), offset[1])),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lines_sit_under_the_stats_block_and_move_the_timing_block_down() {
        let lines = select_lines(
            &read(
                &status_json("idle", r#","problems":["camera stalled"]"#),
                NOW,
            ),
            false,
        );
        let mut actors = Vec::new();
        // Whatever the screen drew before the overlay must not move.
        stats_overlay::push(&mut actors, BackendType::OpenGL, 1.0, 1, None);
        let start = actors.len();
        stats_overlay::push(
            &mut actors,
            BackendType::OpenGL,
            340.0,
            2154,
            Some(timing()),
        );
        let before = rows(&actors);

        recorder_status::push(&mut actors, start, &lines);

        let after = rows(&actors);
        assert_eq!(after.len(), 5);
        assert_eq!(after[..2], before[..2]);
        assert_eq!(after[2].1, before[2].1 + 32.0, "timing block moves down");
        assert_eq!(after[3], ("REC READY".to_owned(), 64.0));
        assert_eq!(after[4], ("camera stalled".to_owned(), 80.0));
    }

    #[test]
    fn no_lines_leaves_the_stats_overlay_unchanged() {
        let mut actors = Vec::new();
        stats_overlay::push(
            &mut actors,
            BackendType::OpenGL,
            340.0,
            2154,
            Some(timing()),
        );
        let before = rows(&actors);
        recorder_status::push(&mut actors, 0, &[]);
        assert_eq!(rows(&actors), before);
    }
}
