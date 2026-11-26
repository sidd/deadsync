//! sidd fork: publish song start/end events and per-play telemetry for the
//! external recorder.
//!
//! Opt-in: nothing happens unless `<data dir>/save/play-events/` exists.
//! `events.jsonl` in that directory receives one `stage_start` and one
//! `stage_end` line per play; `<play_id>.json` holds the full telemetry and is
//! complete before its `stage_end` line appears. While a play is reported, live
//! snapshots also go to the recorder as UDP datagrams (see `live`). Practice is
//! not reported.
//!
//! Hooks run on the game thread and only collect data. Worker threads do the
//! serialization and all file and socket I/O.
//!
//! Never serialize a whole `Profile`: it holds online-service credentials.

use deadsync_assets::gameplay_stage::GameplayCoreState;
use deadsync_core::input::MAX_PLAYERS;
use deadsync_profile::app_runtime::{cached_local_ex_score_for_id, cached_local_itg_score_for_id};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod live;
mod pads;

const SCHEMA: u32 = 1;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

thread_local! {
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

/// Set the output directory. Call once, on the game thread.
pub(crate) fn init(dir: PathBuf) {
    crate::recorder_status::init(dir.clone());
    TRACKER.with_borrow_mut(|tracker| *tracker = Some(Tracker::new(dir)));
}

fn with_tracker(f: impl FnOnce(&mut Tracker)) {
    TRACKER.with_borrow_mut(|tracker| {
        if let Some(tracker) = tracker {
            f(tracker);
        }
    });
}

pub(crate) fn started(gs: &GameplayCoreState, pack_group: &str) {
    with_tracker(|tracker| tracker.stage_started(gs, pack_group));
}

pub(crate) fn frame(gs: &GameplayCoreState) {
    with_tracker(|tracker| tracker.stage_frame(Instant::now(), gs));
}

/// How long the reported play has been running, if there is one.
pub(crate) fn play_age() -> Option<Duration> {
    TRACKER.with_borrow(|tracker| tracker.as_ref()?.play_age(Instant::now()))
}

/// Close the active play. Later calls for the same play do nothing.
pub(crate) fn ended(gs: &GameplayCoreState, restarted: bool) {
    with_tracker(|tracker| tracker.stage_ended(gs, restarted));
}

struct Active {
    play_id: String,
    metadata: Value,
    audio_at_start: Value,
    started: Instant,
    /// `[wall_us, music_time_ns, frames, longest_frame_us]`
    samples: Vec<[i64; 4]>,
    last_frame: Option<Instant>,
    last_sample: Option<Instant>,
    frames: i64,
    longest_frame: Duration,
}

enum Job {
    Event(Value),
    Telemetry { file: String, body: Value },
}

struct Tracker {
    dir: PathBuf,
    active: Option<Active>,
    writer: Option<(mpsc::Sender<Job>, JoinHandle<()>)>,
    sequence: u32,
    live: live::Publisher,
}

impl Tracker {
    fn stage_started(&mut self, gs: &GameplayCoreState, pack_group: &str) {
        if !self.enabled() {
            return;
        }
        self.begin(metadata(gs, pack_group), audio_output());
        if let Some(active) = &self.active {
            // The worker turns on sensor test mode: the call takes the SDK's
            // lock. Only the recorder's values change; the player's own FSR
            // display stays as their option set it.
            let pressure = if crate::recorder_status::pad_pressure_requested() {
                pads::pressure_slots(gs)
            } else {
                0
            };
            self.live.start(&self.dir, &active.play_id, pressure);
        }
    }

    fn stage_frame(&mut self, now: Instant, gs: &GameplayCoreState) {
        if self.active.is_none() {
            return;
        }
        self.frame(now, gs.current_music_time_ns());
        self.live.frame(now, gs);
    }

    fn stage_ended(&mut self, gs: &GameplayCoreState, restarted: bool) {
        if self.active.is_none() {
            return;
        }
        let outcome = if restarted {
            "restarted"
        } else if gs.song_completed_naturally() {
            "completed"
        } else if gs.all_joined_players_failed() {
            "failed"
        } else {
            "quit"
        };
        self.live.finish(gs);
        let (summary, telemetry) = results(gs);
        self.end(outcome, summary, telemetry, audio_output());
    }

    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            active: None,
            writer: None,
            sequence: 0,
            live: live::Publisher::default(),
        }
    }

    fn enabled(&self) -> bool {
        self.dir.is_dir()
    }

    fn play_age(&self, now: Instant) -> Option<Duration> {
        self.active
            .as_ref()
            .map(|active| now.saturating_duration_since(active.started))
    }

    fn begin(&mut self, metadata: Value, audio: Value) {
        // A play that was never closed has no results left to report.
        if self.active.is_some() {
            self.end("replaced", Value::Null, Value::Null, Value::Null);
        }
        let wall_us = wall_us();
        self.sequence = self.sequence.wrapping_add(1);
        let play_id = format!("{}-{}", wall_us / 1000, self.sequence);
        self.send(Job::Event(json!({
            "schema": SCHEMA,
            "event": "stage_start",
            "play_id": play_id,
            "wall_us": wall_us,
            "play": metadata,
        })));
        self.active = Some(Active {
            play_id,
            metadata,
            audio_at_start: audio,
            started: Instant::now(),
            samples: Vec::with_capacity(2048),
            last_frame: None,
            last_sample: None,
            frames: 0,
            longest_frame: Duration::ZERO,
        });
    }

    fn frame(&mut self, now: Instant, music_time_ns: i64) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if let Some(last) = active.last_frame {
            active.longest_frame = active.longest_frame.max(now.duration_since(last));
        }
        active.last_frame = Some(now);
        active.frames += 1;
        if active
            .last_sample
            .is_some_and(|last| now.duration_since(last) < SAMPLE_INTERVAL)
        {
            return;
        }
        active.samples.push([
            wall_us(),
            music_time_ns,
            active.frames,
            i64::try_from(active.longest_frame.as_micros()).unwrap_or(i64::MAX),
        ]);
        active.last_sample = Some(now);
        active.frames = 0;
        active.longest_frame = Duration::ZERO;
    }

    fn end(&mut self, outcome: &str, summary: Value, telemetry: Value, audio: Value) {
        let Some(active) = self.active.take() else {
            return;
        };
        let wall_us = wall_us();
        let file = format!("{}.json", active.play_id);
        self.send(Job::Telemetry {
            file: file.clone(),
            body: json!({
                "schema": SCHEMA,
                "play_id": active.play_id,
                "outcome": outcome,
                "ended_wall_us": wall_us,
                "play": active.metadata,
                "summary": summary,
                "players": telemetry,
                "audio": { "start": active.audio_at_start, "end": audio },
                "sample_fields": ["wall_us", "music_time_ns", "frames", "longest_frame_us"],
                "samples": active.samples,
            }),
        });
        self.send(Job::Event(json!({
            "schema": SCHEMA,
            "event": "stage_end",
            "play_id": active.play_id,
            "wall_us": wall_us,
            "outcome": outcome,
            "telemetry": file,
            "summary": summary,
        })));
    }

    fn send(&mut self, job: Job) {
        if self.writer.is_none() {
            let (sender, receiver) = mpsc::channel();
            let dir = self.dir.clone();
            match std::thread::Builder::new()
                .name("play-events".into())
                .spawn(move || write_jobs(&dir, receiver))
            {
                Ok(thread) => self.writer = Some((sender, thread)),
                Err(error) => {
                    log::warn!("Failed to start the play events writer: {error}");
                    return;
                }
            }
        }
        if let Some((sender, _)) = &self.writer
            && sender.send(job).is_err()
        {
            log::warn!("Play events writer stopped; dropping an event.");
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if let Some((sender, thread)) = self.writer.take() {
            drop(sender);
            let _ = thread.join();
        }
    }
}

fn write_jobs(dir: &Path, jobs: mpsc::Receiver<Job>) {
    for job in jobs {
        let result = match job {
            Job::Event(event) => append_line(&dir.join("events.jsonl"), &event),
            Job::Telemetry { file, body } => write_atomic(dir, &file, &body),
        };
        if let Err(error) = result {
            log::warn!("Failed to write play events in {}: {error}", dir.display());
        }
    }
}

fn append_line(path: &Path, event: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

fn write_atomic(dir: &Path, file: &str, body: &Value) -> std::io::Result<()> {
    let temporary = dir.join(format!(".{file}.tmp"));
    fs::write(&temporary, serde_json::to_vec(body)?)?;
    fs::rename(temporary, dir.join(file))
}

fn wall_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX)
        })
}

/// A song folder need not be valid UTF-8, and `json!` panics on a path that is
/// not, so such bytes are replaced.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn debug(value: impl std::fmt::Debug) -> String {
    format!("{value:?}")
}

fn audio_output() -> Value {
    let timing = deadsync_audio_stream::get_output_timing_snapshot();
    json!({
        "backend": debug(timing.backend),
        "output_mode": timing.requested_output_mode.as_str(),
        "sample_rate_hz": timing.sample_rate_hz,
        "buffer_frames": timing.buffer_frames,
        "estimated_output_delay_ns": timing.estimated_output_delay_ns,
        "underrun_count": timing.underrun_count,
    })
}

/// Everything known when the song starts.
fn metadata(gs: &GameplayCoreState, pack_group: &str) -> Value {
    let song = gs.song();
    let players: Vec<Value> = (0..gs.num_players().min(MAX_PLAYERS))
        .map(|player| {
            let chart = &gs.charts()[player];
            let profile = &gs.profiles()[player];
            let side = gs.setup.session.runtime_player_side(player);
            let profile_id = gs.setup.session.active_profile_id_for_side(side);
            let totals = gs.stage_totals_for_player(player);
            json!({
                "side": debug(side),
                "profile_id": profile_id,
                "previous_best": previous_best(profile_id.as_deref(), &chart.short_hash),
                "profile_name": profile.display_name,
                "chart": {
                    "hash": chart.short_hash,
                    "type": chart.chart_type,
                    "difficulty": chart.difficulty,
                    "meter": chart.meter,
                    "name": chart.chart_name,
                    "description": chart.description,
                    "step_artist": chart.step_artist,
                    "max_nps": chart.max_nps,
                    "breakdown": chart.detailed_breakdown,
                    "breakdown_partial": chart.partial_breakdown,
                    "breakdown_simple": chart.simple_breakdown,
                    "total_streams": chart.total_streams,
                    "total_measures": chart.total_measures,
                    // Unscaled by the music rate, as are the density arrays.
                    "min_bpm": chart.min_bpm,
                    "max_bpm": chart.max_bpm,
                    "measure_nps": chart.measure_nps_vec.iter()
                        .map(|&nps| milli(nps)).collect::<Vec<_>>(),
                    "measure_seconds": chart.measure_seconds_vec.iter()
                        .map(|&second| milli(second.into())).collect::<Vec<_>>(),
                    // Counted after chart-changing options.
                    "totals": {
                        "steps": totals.total_steps,
                        "holds": totals.holds_total,
                        "rolls": totals.rolls_total,
                        "mines": totals.mines_total,
                        "possible_grade_points": totals.possible_grade_points,
                        "jumps": chart.stats.jumps,
                        "hands": chart.stats.hands,
                    },
                },
                "options": {
                    "scroll_speed": debug(profile.scroll_speed),
                    "scroll": debug(profile.scroll_option),
                    "turn": debug(profile.turn_option),
                    "perspective": debug(profile.perspective),
                    "mini_percent": profile.mini_percent,
                    "noteskin": debug(&profile.noteskin),
                    "visual_delay_ms": profile.visual_delay_ms,
                    "global_offset_shift_ms": profile.global_offset_shift_ms,
                },
            })
        })
        .collect();
    json!({
        "song": {
            "title": song.title,
            "subtitle": song.subtitle,
            "artist": song.artist,
            "title_translit": song.translit_title,
            "artist_translit": song.translit_artist,
            "genre": song.genre,
            "display_bpm": song.display_bpm,
            "length_seconds": song.music_length_seconds,
            "first_second": song.precise_first_second(),
            "last_second": song.precise_last_second(),
            "min_bpm": song.min_bpm,
            "max_bpm": song.max_bpm,
            "simfile_path": path_text(&song.simfile_path),
            "music_path": song.music_path.as_deref().map(path_text),
            "banner_path": song.banner_path.as_deref().map(path_text),
        },
        "pack": pack_group,
        "play_style": debug(gs.setup.session.play_style),
        "music_rate": gs.music_rate(),
        "global_offset_seconds": gs.global_offset_seconds(),
        "song_offset_seconds": gs.song_offset_seconds(),
        "timing_windows_seconds": gs.timing_profile_windows_s(),
        "cols_per_player": gs.cols_per_player(),
        "players": players,
    })
}

/// The profile's best local scores on a chart from before this play. Reads the
/// score cache that gameplay setup has already loaded.
fn previous_best(profile_id: Option<&str>, chart_hash: &str) -> Value {
    let itg = profile_id.and_then(|id| cached_local_itg_score_for_id(id, chart_hash));
    let ex = profile_id.and_then(|id| cached_local_ex_score_for_id(id, chart_hash));
    json!({
        "itg": itg.map(|best| json!({
            "percent": best.score_percent * 100.0,
            "grade": debug(best.grade),
        })),
        "ex": ex.map(|best| json!({ "percent": best.percent, "failed": best.is_fail })),
    })
}

fn milli(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Final scores, plus note-by-note and input telemetry for each player.
fn results(gs: &GameplayCoreState) -> (Value, Value) {
    let stage = deadsync_profile_gameplay::results::stage_result(gs, Default::default());
    let summary: Vec<Value> = stage
        .players
        .iter()
        .enumerate()
        .filter_map(|(side, player)| {
            let player = player.as_ref()?;
            Some(json!({
                "side": side + 1,
                "profile_name": player.profile_name,
                "grade": debug(player.grade),
                "score_percent": player.score_percent,
                "ex_score_percent": player.ex_score_percent,
                "hard_ex_score_percent": player.hard_ex_score_percent,
                "score_valid": player.score_valid,
                "disqualified": player.disqualified,
                "fail_time": player.fail_time,
            }))
        })
        .collect();
    let telemetry: Vec<Value> = (0..gs.num_players().min(MAX_PLAYERS))
        .map(|player| player_telemetry(gs, player))
        .collect();
    (
        json!({
            "duration_seconds": stage.duration_seconds,
            "autoplay_used": gs.autoplay_used(),
            "players": summary,
        }),
        Value::Array(telemetry),
    )
}

fn player_telemetry(gs: &GameplayCoreState, player: usize) -> Value {
    let state = &gs.players()[player];
    let (start, end) = gs.note_range_for_player(player);
    let first_column = player * gs.cols_per_player();
    let judgment = |judgment: &deadsync_rules::judgment::Judgment| {
        json!({
            "grade": debug(judgment.grade),
            "window": judgment.window.map(debug),
            "offset_ms": judgment.time_error_ms,
            "miss_because_held": judgment.miss_because_held,
        })
    };
    let notes: Vec<Value> = (start..end)
        .map(|index| {
            let note = &gs.notes()[index];
            json!({
                "time_ns": gs.note_time_cache_ns().get(index),
                "beat": note.beat,
                "column": note.column.checked_sub(first_column).unwrap_or(note.column),
                "type": debug(note.note_type),
                "judgable": note.can_be_judged,
                "result": note.result.as_ref().map(judgment),
                "early_result": note.early_result.as_ref().map(judgment),
                "mine": note.mine_result.map(debug),
                "hold": note.hold.as_ref().map(|hold| json!({
                    "end_beat": hold.end_beat,
                    "end_time_ns": gs.hold_end_time_cache_ns().get(index),
                    "result": hold.result.map(debug),
                })),
            })
        })
        .collect();
    let lanes = first_column..first_column + gs.cols_per_player();
    let inputs: Vec<Value> = gs
        .recorded_replay_edges()
        .iter()
        // One player owns every lane, including both pads in Double.
        .filter(|edge| gs.num_players() == 1 || lanes.contains(&usize::from(edge.lane_index)))
        .map(|edge| {
            json!([
                edge.event_music_time_ns,
                edge.lane_index,
                edge.pressed,
                debug(edge.source),
            ])
        })
        .collect();
    json!({
        "judgment_order": ["Fantastic", "Excellent", "Great", "Decent", "WayOff", "Miss"],
        "judgment_counts": state.judgment_counts,
        "max_combo": state.recorded_max_combo,
        "holds_held": state.holds_held,
        "rolls_held": state.rolls_held,
        "mines_hit": state.mines_hit,
        "mines_avoided": state.mines_avoided,
        "hands_achieved": state.hands_achieved,
        "calories_burned": state.calories_burned,
        "final_life": state.life,
        "life_history": state.life_history,
        "notes": notes,
        "input_fields": ["music_time_ns", "lane", "pressed", "source"],
        "inputs": inputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_events(dir: &Path) -> Vec<Value> {
        fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn stage_start_describes_the_chart_without_credentials() {
        const SECRET: &str = "credential-that-must-stay-private";
        let gs = live::tests::state_with(Default::default(), |profile| {
            profile.groovestats_api_key = SECRET.into();
            profile.arrowcloud_api_key = SECRET.into();
            profile.groovestats_username = SECRET.into();
        });

        let play = metadata(&gs, "Pack");

        assert!(!play.to_string().contains(SECRET));
        let song = &play["song"];
        assert_eq!(song["min_bpm"], 150.0);
        assert_eq!(song["max_bpm"], 150.0);
        assert!(song["last_second"].as_f64().unwrap() > 0.0);
        assert!(song["first_second"].is_number());
        let chart = &play["players"][0]["chart"];
        assert_eq!(chart["min_bpm"], 150.0);
        assert_eq!(chart["max_bpm"], 150.0);
        let nps = chart["measure_nps"].as_array().unwrap();
        let seconds = chart["measure_seconds"].as_array().unwrap();
        assert_eq!(nps.len(), 2);
        assert_eq!(seconds.len(), 2);
        // 150 BPM: a measure is 1.6 s, and the first holds four notes.
        assert_eq!(nps[0], 2.5);
        assert_eq!(seconds[1], 1.6);
        assert_eq!(chart["total_measures"], gs.charts()[0].total_measures);
        assert_eq!(chart["total_streams"], 0);
        for key in ["breakdown", "breakdown_partial", "breakdown_simple"] {
            assert!(chart[key].is_string(), "{key}");
        }
        assert_eq!(
            chart["totals"],
            json!({"steps": 7, "holds": 1, "rolls": 0, "mines": 1,
                   "possible_grade_points": 40, "jumps": 0, "hands": 0})
        );
        assert_eq!(
            play["players"][0]["previous_best"],
            json!({"itg": null, "ex": null})
        );
    }

    #[test]
    fn a_reported_play_publishes_live_snapshots_under_its_play_id() {
        let (dir, socket) = live::tests::listener();
        let gs = live::tests::state();
        let mut tracker = Tracker::new(dir.path().to_path_buf());

        tracker.stage_started(&gs, "Pack");
        tracker.stage_frame(Instant::now(), &gs);
        let first = live::tests::receive(&socket);
        tracker.stage_ended(&gs, false);
        let last = live::tests::receive(&socket);
        drop(tracker);

        let events = read_events(dir.path());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["play"]["pack"], "Pack");
        assert_eq!(events[1]["outcome"], "quit");
        assert_eq!(first["play_id"], events[0]["play_id"]);
        assert_eq!(last["play_id"], events[0]["play_id"]);
        assert_eq!(last["ended"], true);
    }

    #[test]
    fn nothing_happens_without_the_play_events_directory() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("play-events");
        let gs = live::tests::state();
        let mut tracker = Tracker::new(missing.clone());

        tracker.stage_started(&gs, "Pack");
        tracker.stage_frame(Instant::now(), &gs);
        tracker.stage_ended(&gs, false);

        assert!(tracker.active.is_none());
        assert!(tracker.writer.is_none());
        assert!(!tracker.live.is_running());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_still_reported() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/songs/caf\xe9/a.ogg"));
        assert_eq!(json!(path_text(path)), "/songs/caf\u{fffd}/a.ogg");
    }

    #[test]
    fn disabled_until_the_directory_exists() {
        let root = tempfile::tempdir().unwrap();
        let tracker = Tracker::new(root.path().join("play-events"));
        assert!(!tracker.enabled());
        fs::create_dir(root.path().join("play-events")).unwrap();
        assert!(tracker.enabled());
    }

    #[test]
    fn a_play_is_reported_once_with_its_telemetry_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(dir.path().to_path_buf());
        tracker.begin(json!({"song": "a"}), Value::Null);
        let now = Instant::now();
        tracker.frame(now, 1);
        tracker.frame(now + Duration::from_millis(10), 2);
        tracker.frame(now + Duration::from_millis(300), 3);
        tracker.end("completed", json!({"ok": true}), json!([]), Value::Null);
        tracker.end("quit", Value::Null, Value::Null, Value::Null);
        drop(tracker);

        let events = read_events(dir.path());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "stage_start");
        assert_eq!(events[1]["event"], "stage_end");
        assert_eq!(events[1]["outcome"], "completed");
        assert_eq!(events[0]["play_id"], events[1]["play_id"]);
        let file = events[1]["telemetry"].as_str().unwrap();
        let telemetry: Value =
            serde_json::from_slice(&fs::read(dir.path().join(file)).unwrap()).unwrap();
        let samples = telemetry["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[1][1], 3);
        assert_eq!(samples[1][2], 2);
        assert_eq!(samples[1][3], 290_000);
    }

    #[test]
    fn play_age_counts_from_the_start_of_the_active_play() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(dir.path().to_path_buf());
        assert_eq!(tracker.play_age(Instant::now()), None);
        let before = Instant::now();
        tracker.begin(Value::Null, Value::Null);
        let after = Instant::now();
        let later = after + Duration::from_secs(4);
        let age = tracker.play_age(later).unwrap();
        assert!(age >= Duration::from_secs(4) && age <= later - before);
        tracker.end("quit", Value::Null, Value::Null, Value::Null);
        assert_eq!(tracker.play_age(later), None);
    }

    #[test]
    fn a_new_play_closes_an_unfinished_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(dir.path().to_path_buf());
        tracker.begin(Value::Null, Value::Null);
        tracker.begin(Value::Null, Value::Null);
        drop(tracker);

        let events = read_events(dir.path());
        assert_eq!(events.len(), 3);
        assert_eq!(events[1]["outcome"], "replaced");
        assert_ne!(events[0]["play_id"], events[2]["play_id"]);
    }
}
