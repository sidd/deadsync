//! sidd fork: live gameplay snapshots for the recorder's overlay feed.
//!
//! The game thread copies counters into a plain [`Snapshot`] about 20 times a
//! second, and at once after a judgment. It leaves the copy in a one-value
//! mailbox it only ever `try_lock`s: during a song nothing on the game thread
//! waits, allocates, serializes, wakes a thread or touches a socket. A worker
//! polls the mailbox, does the score arithmetic with the HUD's own functions,
//! encodes JSON and sends one UDP datagram to the recorder on this machine.
//! Anything not accepted is dropped.
//!
//! When the recorder wants pad pressure, the worker also turns on the SMX
//! sensor test mode, since that call takes the SDK's lock (see
//! [`PressureGate`]).
//!
//! Message format: the recorder's API.md.

use super::pads::{self, Pads, SlotSensors};
use deadsync_assets::gameplay_stage::GameplayCoreState;
use deadsync_core::input::MAX_PLAYERS;
use deadsync_gameplay::{
    GameplayInputPlayerSide, GameplayScoreDisplayMode, ItgScoreInputs,
    display_ex_score_percent_for_mode, display_hard_ex_score_percent_for_mode,
    display_itg_score_percent_for_mode, finalized_row_judgment_for_entry,
};
use deadsync_profile_gameplay::score_display_mode_from_profile;
use deadsync_rules::judgment::{
    ExScoreData, JudgeCounts, JudgeGrade, Judgment, TimingWindow, add_judgment_to_window_counts,
};
use deadsync_rules::timing::{LiveTimingSnapshot, WindowCounts};
use serde::Serialize;
use std::net::{Ipv4Addr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SCHEMA: u32 = 1;
/// Used when the recorder's status file names no port.
const DEFAULT_PORT: u16 = 8735;
/// Time between periodic snapshots.
const INTERVAL: Duration = Duration::from_millis(50);
/// Judgments are sent at once, but never closer together than this.
const MIN_GAP: Duration = Duration::from_millis(5);
/// How often the worker looks for a snapshot during a play.
const POLL: Duration = Duration::from_millis(2);
const PLAY_ID_BYTES: usize = 40;

/// A play id held inline, so copying it never allocates.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PlayId {
    bytes: [u8; PLAY_ID_BYTES],
    len: usize,
}

impl PlayId {
    fn new(id: &str) -> Self {
        let mut len = id.len().min(PLAY_ID_BYTES);
        while !id.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; PLAY_ID_BYTES];
        bytes[..len].copy_from_slice(&id.as_bytes()[..len]);
        Self { bytes, len }
    }

    fn as_str(&self) -> &str {
        // Cut on a character boundary of a string, so always valid.
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

/// Everything copied off the game thread for one datagram.
#[derive(Clone, Copy)]
pub(super) struct Snapshot {
    play_id: PlayId,
    seq: u64,
    ended: bool,
    wall_us: i64,
    music_time_ns: i64,
    song_length: f32,
    music_rate: f32,
    beat: f32,
    /// Chart BPM at the displayed beat, before the music rate.
    bpm: f32,
    num_players: usize,
    players: [Player; MAX_PLAYERS],
    pads: Pads,
}

#[derive(Clone, Copy)]
struct Player {
    side: GameplayInputPlayerSide,
    score_mode: GameplayScoreDisplayMode,
    show_ex_score: bool,
    show_hard_ex_score: bool,
    itg: Option<ItgScoreInputs>,
    ex: ExScoreData,
    life: f32,
    failing: bool,
    failed: bool,
    combo: u32,
    miss_combo: u32,
    max_combo: u32,
    full_combo_grade: Option<JudgeGrade>,
    judgments: JudgeCounts,
    blue_window_ms: f32,
    windows: WindowCounts,
    holds_held: u32,
    holds_total: u32,
    rolls_held: u32,
    rolls_total: u32,
    mines_hit: u32,
    mines_avoided: u32,
    mines_total: u32,
    timing: LiveTimingSnapshot,
    early: u32,
    late: u32,
    /// The displayed judgment and its number within the play.
    last_judgment: Option<(u64, Judgment)>,
}

/// What makes a player's state worth sending at once.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Mark {
    /// Screen time, offset and grade of the displayed judgment.
    judgment: Option<(u32, u32, JudgeGrade)>,
    counts: JudgeCounts,
    holds_held: u32,
    rolls_held: u32,
    mines_hit: u32,
    combo: u32,
    failing: bool,
}

/// What has to be followed from frame to frame for one player.
#[derive(Clone, Copy, Default)]
struct Track {
    mark: Mark,
    judgments: u64,
    /// First chart row not yet counted as early or late.
    row: usize,
    early: u32,
    late: u32,
}

impl Track {
    /// Take in this frame's changes. True after a new judgment or similar.
    fn follow(&mut self, gs: &GameplayCoreState, player: usize) -> bool {
        let state = &gs.players()[player];
        let judgment = state.last_judgment.map(|shown| {
            (
                shown.started_at_screen_s.to_bits(),
                shown.judgment.time_error_ms.to_bits(),
                shown.judgment.grade,
            )
        });
        if judgment.is_some() && judgment != self.mark.judgment {
            self.judgments += 1;
        }
        // Rows before the game's cursor are final. Each is visited once.
        let rows = &gs.chart_runtime.row_entries;
        let (start, end) = gs.chart_runtime.row_indices.row_entry_ranges[player];
        let judged = gs.chart_runtime.row_indices.judged_row_cursor[player]
            .min(end)
            .min(rows.len());
        let newly_judged = rows.get(self.row.max(start)..judged).unwrap_or_default();
        for entry in newly_judged {
            let Some(row) = finalized_row_judgment_for_entry(&gs.chart_runtime.notes, entry) else {
                continue;
            };
            if row.judgment.grade == JudgeGrade::Miss {
                continue;
            }
            if row.judgment.time_error_ms < 0.0 {
                self.early += 1;
            } else if row.judgment.time_error_ms > 0.0 {
                self.late += 1;
            }
        }
        self.row = self.row.max(judged);

        let mark = Mark {
            judgment,
            counts: state.judgment_counts,
            holds_held: state.holds_held,
            rolls_held: state.rolls_held,
            mines_hit: state.mines_hit,
            combo: state.combo,
            failing: state.is_failing,
        };
        let changed = mark != self.mark;
        self.mark = mark;
        changed
    }

    /// Copy what one datagram says about a player. The counters are the ones
    /// the HUD and step statistics draw, including any course carry-over.
    fn capture(&self, gs: &GameplayCoreState, player: usize) -> Player {
        let state = &gs.players()[player];
        let profile = &gs.profiles()[player];
        let carry = gs.display_carry_for_player(player);
        let totals = gs.display_totals_for_player(player);
        // The player's own split reads a running counter; no notes are scanned.
        let blue_window_ms = gs.player_blue_window_ms(player);
        let grade = |grade| gs.display_judgment_count(player, grade);
        Player {
            side: gs.setup.session.runtime_player_side(player),
            score_mode: score_display_mode_from_profile(profile.score_display_mode),
            show_ex_score: profile.show_ex_score,
            show_hard_ex_score: profile.show_ex_score && profile.show_hard_ex_score,
            itg: gs.display_itg_score_inputs(player),
            ex: gs.display_scored_ex_score_data(player, blue_window_ms),
            life: state.life,
            failing: state.is_failing,
            failed: state.stage_failed || gs.player_is_dead(player),
            combo: state.combo,
            miss_combo: state.miss_combo,
            max_combo: state.recorded_max_combo,
            full_combo_grade: state.full_combo_grade,
            judgments: [
                grade(JudgeGrade::Fantastic),
                grade(JudgeGrade::Excellent),
                grade(JudgeGrade::Great),
                grade(JudgeGrade::Decent),
                grade(JudgeGrade::WayOff),
                grade(JudgeGrade::Miss),
            ],
            blue_window_ms,
            windows: gs.display_window_counts(player, Some(blue_window_ms), blue_window_ms),
            holds_held: state.holds_held.saturating_add(carry.holds_held),
            holds_total: totals.holds_total,
            rolls_held: state.rolls_held.saturating_add(carry.rolls_held),
            rolls_total: totals.rolls_total,
            mines_hit: state.mines_hit,
            mines_avoided: state.mines_avoided.saturating_add(carry.mines_avoided),
            mines_total: totals.mines_total,
            timing: gs.display_live_timing_stats(player),
            early: self.early,
            late: self.late,
            last_judgment: state
                .last_judgment
                .map(|shown| (self.judgments, shown.judgment)),
        }
    }
}

/// Per-play state owned by the game thread.
struct Play {
    id: PlayId,
    seq: u64,
    /// When the next periodic snapshot is due; `None` means now.
    next_due: Option<Instant>,
    last_sent: Option<Instant>,
    /// Something happened that has not been sent yet.
    pending: bool,
    tracks: [Track; MAX_PLAYERS],
    pads: Pads,
    /// Reads an SMX pad slot's panel bits.
    smx: fn(usize) -> u16,
}

impl Play {
    fn new(play_id: &str) -> Self {
        Self {
            id: PlayId::new(play_id),
            seq: 0,
            next_due: None,
            last_sent: None,
            pending: false,
            tracks: [Track::default(); MAX_PLAYERS],
            pads: Pads::default(),
            smx: pads::smx_panel_bits,
        }
    }

    fn follow(&mut self, gs: &GameplayCoreState) {
        for player in 0..gs.num_players().min(MAX_PLAYERS) {
            self.pending |= self.tracks[player].follow(gs, player);
        }
        let pads = Pads::capture(gs, self.smx);
        self.pending |= pads != self.pads;
        self.pads = pads;
    }

    /// Call every frame. Returns a snapshot when one should be sent.
    fn observe(&mut self, now: Instant, gs: &GameplayCoreState) -> Option<Snapshot> {
        self.follow(gs);
        let periodic = self.next_due.is_none_or(|due| now >= due);
        let urgent = self.pending
            && self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= MIN_GAP);
        if !periodic && !urgent {
            return None;
        }
        // Periodic snapshots keep their own beat so that frame times do not
        // stretch the interval; anything else starts a new one.
        let next = match self.next_due {
            Some(due) if periodic && now < due + INTERVAL => due + INTERVAL,
            _ => now + INTERVAL,
        };
        self.next_due = Some(next);
        self.last_sent = Some(now);
        Some(self.snapshot(gs, false))
    }

    /// The last snapshot was not accepted: offer a fresh one next frame.
    fn retry(&mut self) {
        self.next_due = None;
    }

    fn finish(&mut self, gs: &GameplayCoreState) -> Snapshot {
        self.follow(gs);
        self.snapshot(gs, true)
    }

    fn snapshot(&mut self, gs: &GameplayCoreState, ended: bool) -> Snapshot {
        self.seq += 1;
        self.pending = false;
        let num_players = gs.num_players().min(MAX_PLAYERS);
        // Unused slots repeat the first player and are never encoded.
        let first = self.tracks[0].capture(gs, 0);
        let mut players = [first; MAX_PLAYERS];
        for (player, slot) in players.iter_mut().enumerate().take(num_players).skip(1) {
            *slot = self.tracks[player].capture(gs, player);
        }
        Snapshot {
            play_id: self.id,
            seq: self.seq,
            ended,
            wall_us: super::wall_us(),
            music_time_ns: gs.current_music_time_ns(),
            song_length: gs.song().music_length_seconds,
            music_rate: gs.music_rate(),
            beat: gs.current_beat_display(),
            bpm: gs.current_bpm_display(),
            num_players,
            players,
            pads: self.pads,
        }
    }
}

#[derive(Serialize)]
struct Message<'a> {
    schema: u32,
    #[serde(rename = "type")]
    kind: &'static str,
    play_id: &'a str,
    seq: u64,
    ended: bool,
    wall_us: i64,
    music_time: f64,
    song_length: f32,
    music_rate: f32,
    beat: f32,
    bpm: f64,
    players: Vec<PlayerMessage>,
    pads: Vec<pads::PadMessage>,
}

#[derive(Serialize)]
struct PlayerMessage {
    side: &'static str,
    score_mode: &'static str,
    show_ex_score: bool,
    show_hard_ex_score: bool,
    itg_percent: f64,
    ex_percent: f64,
    hard_ex_percent: f64,
    life: f32,
    failing: bool,
    failed: bool,
    combo: u32,
    miss_combo: u32,
    max_combo: u32,
    full_combo_grade: Option<&'static str>,
    judgments: Judgments,
    fa_plus: FaPlus,
    holds: Held,
    rolls: Held,
    mines: Mines,
    timing: Timing,
    last_judgment: Option<LastJudgment>,
}

#[derive(Serialize)]
struct Judgments {
    fantastic: u32,
    excellent: u32,
    great: u32,
    decent: u32,
    way_off: u32,
    miss: u32,
}

#[derive(Serialize)]
struct FaPlus {
    blue_window_ms: f32,
    w0: u32,
    w1: u32,
    w2: u32,
    w3: u32,
    w4: u32,
    w5: u32,
    miss: u32,
}

#[derive(Serialize)]
struct Held {
    held: u32,
    total: u32,
}

#[derive(Serialize)]
struct Mines {
    hit: u32,
    avoided: u32,
    total: u32,
}

#[derive(Serialize)]
struct Timing {
    early: u32,
    late: u32,
    mean_ms: f32,
    mean_abs_ms: f32,
    stddev_ms: f32,
    max_abs_ms: f32,
    recent_mean_ms: f32,
    recent_mean_abs_ms: f32,
}

#[derive(Serialize)]
struct LastJudgment {
    seq: u64,
    grade: &'static str,
    window: Option<&'static str>,
    blue: bool,
    offset_ms: Option<f32>,
    miss_because_held: bool,
}

const fn grade_name(grade: JudgeGrade) -> &'static str {
    match grade {
        JudgeGrade::Fantastic => "Fantastic",
        JudgeGrade::Excellent => "Excellent",
        JudgeGrade::Great => "Great",
        JudgeGrade::Decent => "Decent",
        JudgeGrade::WayOff => "WayOff",
        JudgeGrade::Miss => "Miss",
    }
}

const fn window_name(window: TimingWindow) -> &'static str {
    match window {
        TimingWindow::W0 => "W0",
        TimingWindow::W1 => "W1",
        TimingWindow::W2 => "W2",
        TimingWindow::W3 => "W3",
        TimingWindow::W4 => "W4",
        TimingWindow::W5 => "W5",
    }
}

impl LastJudgment {
    fn new(seq: u64, judgment: Judgment, blue_window_ms: f32) -> Self {
        let hit = judgment.grade != JudgeGrade::Miss;
        let mut split = WindowCounts::default();
        add_judgment_to_window_counts(&mut split, &judgment, blue_window_ms);
        Self {
            seq,
            grade: grade_name(judgment.grade),
            window: judgment.window.filter(|_| hit).map(window_name),
            blue: split.w0 > 0,
            offset_ms: hit.then_some(judgment.time_error_ms),
            miss_because_held: judgment.miss_because_held,
        }
    }
}

impl From<&Player> for PlayerMessage {
    fn from(player: &Player) -> Self {
        let mode = player.score_mode;
        let [fantastic, excellent, great, decent, way_off, miss] = player.judgments;
        let windows = player.windows;
        Self {
            side: match player.side {
                GameplayInputPlayerSide::P1 => "P1",
                GameplayInputPlayerSide::P2 => "P2",
            },
            score_mode: match mode {
                GameplayScoreDisplayMode::Normal => "normal",
                GameplayScoreDisplayMode::Predictive => "predictive",
            },
            show_ex_score: player.show_ex_score,
            show_hard_ex_score: player.show_hard_ex_score,
            itg_percent: player.itg.map_or(0.0, |inputs| {
                display_itg_score_percent_for_mode(inputs, mode)
            }),
            ex_percent: display_ex_score_percent_for_mode(&player.ex, mode).max(0.0),
            hard_ex_percent: display_hard_ex_score_percent_for_mode(&player.ex, mode).max(0.0),
            life: player.life,
            failing: player.failing,
            failed: player.failed,
            combo: player.combo,
            miss_combo: player.miss_combo,
            max_combo: player.max_combo,
            full_combo_grade: player.full_combo_grade.map(grade_name),
            judgments: Judgments {
                fantastic,
                excellent,
                great,
                decent,
                way_off,
                miss,
            },
            fa_plus: FaPlus {
                blue_window_ms: player.blue_window_ms,
                w0: windows.w0,
                w1: windows.w1,
                w2: windows.w2,
                w3: windows.w3,
                w4: windows.w4,
                w5: windows.w5,
                miss: windows.miss,
            },
            holds: Held {
                held: player.holds_held,
                total: player.holds_total,
            },
            rolls: Held {
                held: player.rolls_held,
                total: player.rolls_total,
            },
            mines: Mines {
                hit: player.mines_hit,
                avoided: player.mines_avoided,
                total: player.mines_total,
            },
            timing: Timing {
                early: player.early,
                late: player.late,
                mean_ms: player.timing.all.mean_ms,
                mean_abs_ms: player.timing.all.mean_abs_ms,
                stddev_ms: player.timing.all.stddev_ms,
                max_abs_ms: player.timing.all.max_abs_ms,
                recent_mean_ms: player.timing.recent.mean_ms,
                recent_mean_abs_ms: player.timing.recent.mean_abs_ms,
            },
            last_judgment: player
                .last_judgment
                .map(|(seq, judgment)| LastJudgment::new(seq, judgment, player.blue_window_ms)),
        }
    }
}

/// Replace `out` with the snapshot's JSON.
fn encode(snapshot: &Snapshot, sensors: &SlotSensors, out: &mut Vec<u8>) {
    let rate = f64::from(snapshot.music_rate);
    let bpm = f64::from(snapshot.bpm) * if rate.is_finite() { rate } else { 1.0 };
    let message = Message {
        schema: SCHEMA,
        kind: "live",
        play_id: snapshot.play_id.as_str(),
        seq: snapshot.seq,
        ended: snapshot.ended,
        wall_us: snapshot.wall_us,
        music_time: snapshot.music_time_ns as f64 * 1e-9,
        song_length: snapshot.song_length,
        music_rate: snapshot.music_rate,
        beat: snapshot.beat,
        // What the HUD's BPM display shows.
        bpm: if bpm.is_finite() { bpm } else { 0.0 },
        players: snapshot.players[..snapshot.num_players]
            .iter()
            .map(PlayerMessage::from)
            .collect(),
        pads: pads::messages(&snapshot.pads, sensors),
    };
    out.clear();
    if let Err(error) = serde_json::to_writer(&mut *out, &message) {
        log::debug!("Failed to encode a live snapshot: {error}");
        out.clear();
    }
}

/// The recorder names its port in the status file it keeps in `dir`.
fn port_from_status(dir: &Path) -> u16 {
    std::fs::read(dir.join("recorder-status.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|status| status.get("live_port")?.as_u64())
        .and_then(|port| u16::try_from(port).ok())
        .filter(|&port| port != 0)
        .unwrap_or(DEFAULT_PORT)
}

/// Hands a play's request for pad pressure to the worker.
///
/// Turning test mode on takes the SDK's lock, which can be held for a USB
/// write or a device open, so the game thread only records the request and
/// the worker makes the call. Leaving gameplay turns test mode off on the game
/// thread; `end` makes sure the worker's call cannot land after that.
#[derive(Default)]
pub(super) struct PressureGate {
    /// `generation << 8 | slots` of the newest play; 0 before the first.
    request: AtomicU64,
    /// Newest generation whose play has ended. The worker holds the lock while
    /// it turns test mode on, so a play cannot end halfway through the call.
    ended: Mutex<u64>,
}

impl PressureGate {
    /// Game thread, as a play starts: one atomic store.
    fn request(&self, generation: u64, slots: u8) {
        self.request
            .store((generation << 8) | u64::from(slots), Ordering::Release);
    }

    /// Game thread, as a play that asked for pressure ends, before gameplay
    /// turns test mode off. Waits only while the worker is in its call.
    fn end(&self, generation: u64) {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        *ended = (*ended).max(generation);
    }

    /// Worker: act on a new request once, unless its play has ended.
    fn apply(&self, applied: &mut u64, mut sensor_on: impl FnMut(usize)) {
        let request = self.request.load(Ordering::Acquire);
        let generation = request >> 8;
        if generation == *applied {
            return;
        }
        *applied = generation;
        let slots = request & 0xff;
        if slots == 0 {
            return;
        }
        let ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        if *ended >= generation {
            return;
        }
        for slot in (0..8).filter(|slot| slots & (1 << slot) != 0) {
            sensor_on(slot);
        }
        drop(ended);
    }
}

/// One replaceable value. The game thread never waits for it.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<Snapshot>>,
    /// The slot holds a value, so the worker has a reason to lock it.
    ready: AtomicBool,
    /// A play is running, so the worker polls instead of parking.
    playing: AtomicBool,
    closed: AtomicBool,
    pressure: PressureGate,
}

impl Mailbox {
    /// False if the worker holds the slot right now.
    fn post(&self, snapshot: &Snapshot) -> bool {
        let Ok(mut slot) = self.slot.try_lock() else {
            return false;
        };
        *slot = Some(*snapshot);
        self.ready.store(true, Ordering::Release);
        true
    }

    /// Worker side. Locks only after a post, so that polling can never make the
    /// game thread's `try_lock` fail. The lock is released before the value is
    /// used.
    fn take(&self) -> Option<Snapshot> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        self.ready.store(false, Ordering::Release);
        slot.take()
    }
}

/// Worker thread: encode and send until the mailbox is closed.
fn send_snapshots(dir: &Path, mailbox: &Mailbox, sensor_on: fn(usize)) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).and_then(|socket| {
        socket.set_nonblocking(true)?;
        Ok(socket)
    });
    let socket = match socket {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!("Live play snapshots are off: no UDP socket: {error}");
            return;
        }
    };
    let mut bytes = Vec::with_capacity(4096);
    let mut target: Option<(PlayId, u16)> = None;
    let mut sensors = SlotSensors::default();
    let mut pressure = 0;
    loop {
        // Read before `take`: whatever was posted before a flag changed is
        // then certain to be seen.
        let closed = mailbox.closed.load(Ordering::Acquire);
        let playing = mailbox.playing.load(Ordering::Acquire);
        mailbox.pressure.apply(&mut pressure, sensor_on);
        let Some(snapshot) = mailbox.take() else {
            if closed {
                return;
            }
            if playing {
                std::thread::sleep(POLL);
            } else {
                // An unpark that arrived since the loads makes this return.
                std::thread::park();
            }
            continue;
        };
        // The port is looked up once per play.
        let port = match target {
            Some((id, port)) if id == snapshot.play_id => port,
            _ => {
                let port = port_from_status(dir);
                target = Some((snapshot.play_id, port));
                sensors = pads::connected_pads();
                port
            }
        };
        pads::read_sensors(&mut sensors);
        encode(&snapshot, &sensors, &mut bytes);
        // Best effort: with no recorder listening the datagram is dropped.
        let _ = socket.send_to(&bytes, (Ipv4Addr::LOCALHOST, port));
    }
}

struct Worker {
    mailbox: Arc<Mailbox>,
    thread: JoinHandle<()>,
}

impl Worker {
    fn spawn(dir: &Path, sensor_on: fn(usize)) -> Option<Self> {
        let mailbox = Arc::new(Mailbox::default());
        let shared = Arc::clone(&mailbox);
        let dir = dir.to_path_buf();
        std::thread::Builder::new()
            .name("play-live".into())
            .spawn(move || send_snapshots(&dir, &shared, sensor_on))
            .map_err(|error| log::warn!("Failed to start the live snapshot sender: {error}"))
            .ok()
            .map(|thread| Self { mailbox, thread })
    }

    /// Start or stop polling. Wakes the thread, so only call between songs.
    fn set_playing(&self, playing: bool) {
        self.mailbox.playing.store(playing, Ordering::Release);
        self.thread.thread().unpark();
    }
}

/// Sends the active play's snapshots. The worker starts with the first play.
pub(super) struct Publisher {
    worker: Option<Worker>,
    play: Option<Play>,
    /// Numbers the plays for the pressure gate.
    generation: u64,
    /// A play asked for pressure and has not ended in the gate yet.
    pressure_pending: bool,
    /// Turns on sensor test mode for a pad slot, on the worker.
    sensor_on: fn(usize),
}

impl Default for Publisher {
    fn default() -> Self {
        Self {
            worker: None,
            play: None,
            generation: 0,
            pressure_pending: false,
            sensor_on: pads::start_sensor_test,
        }
    }
}

impl Publisher {
    #[cfg(test)]
    pub(super) fn is_running(&self) -> bool {
        self.worker.is_some()
    }

    /// Begin a play. `dir` is the play-events directory; `pressure_slots`
    /// are the pad slots to read sensor values from (bit n for slot n).
    pub(super) fn start(&mut self, dir: &Path, play_id: &str, pressure_slots: u8) {
        if self.worker.is_none() {
            self.worker = Worker::spawn(dir, self.sensor_on);
        }
        if let Some(worker) = &self.worker {
            // Every play replaces the request, so an older one is never acted on.
            self.generation += 1;
            worker.mailbox.pressure.request(self.generation, pressure_slots);
            self.pressure_pending |= pressure_slots != 0;
            worker.set_playing(true);
        }
        self.play = Some(Play::new(play_id));
    }

    /// Game thread, every frame.
    pub(super) fn frame(&mut self, now: Instant, gs: &GameplayCoreState) {
        if let (Some(play), Some(worker)) = (&mut self.play, &self.worker)
            && let Some(snapshot) = play.observe(now, gs)
            && !worker.mailbox.post(&snapshot)
        {
            play.retry();
        }
    }

    /// Send the play's final state. Later frames send nothing.
    pub(super) fn finish(&mut self, gs: &GameplayCoreState) {
        if let (Some(mut play), Some(worker)) = (self.play.take(), &self.worker) {
            // Dropped if the worker is busy: `stage_end` is the record.
            worker.mailbox.post(&play.finish(gs));
            if std::mem::take(&mut self.pressure_pending) {
                worker.mailbox.pressure.end(self.generation);
            }
            worker.set_playing(false);
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.mailbox.closed.store(true, Ordering::Release);
            worker.thread.thread().unpark();
            let _ = worker.thread.join();
        }
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
pub(super) mod tests;
