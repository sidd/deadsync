use super::*;
use deadsync_gameplay::{
    CourseLifeConfig, GameplayConfig, GameplayInputPlayStyle, GameplayMiniIndicatorData,
    GameplayNoteskinData, GameplayScoreDisplayMode, GameplaySession, GameplayViewport,
};
use deadsync_profile::{Profile, ScoreDisplayMode};
use deadsync_profile_gameplay::GameplayProfile;
use deadsync_rules::judgment::{JudgeGrade, Judgment, TimingWindow};
use deadsync_rules::scroll::ScrollSpeedSetting;
use serde_json::Value;
use std::net::UdpSocket;
use std::sync::Arc;

/// Four taps, then a measure with a hold and a mine.
const SIMFILE: &str = "#TITLE:Live Test;
#ARTIST:Tests;
#OFFSET:0;
#BPMS:0=150;
#NOTES:
     dance-single:
     :
     Challenge:
     10:
     0,0,0,0,0:
1000
0100
0010
0001
,
1000
0100
2000
3M00
;
";

pub(crate) fn state_with(
    play_style: GameplayInputPlayStyle,
    edit: impl Fn(&mut Profile),
) -> GameplayCoreState {
    crate::tests::init_paths();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.sm");
    std::fs::write(&path, SIMFILE).unwrap();
    let song = Arc::new(deadsync_simfile::app_runtime::parse_song_for_test(&path, 0.0).unwrap());
    let payload = Arc::new(
        deadsync_simfile::app_runtime::load_gameplay_charts(&song, &[0], 0.0)
            .unwrap()
            .remove(0),
    );
    let chart = Arc::new(song.charts[0].clone());
    let profiles: [GameplayProfile; MAX_PLAYERS] = std::array::from_fn(|_| {
        let mut profile = Profile::default();
        edit(&mut profile);
        GameplayProfile::from(profile)
    });
    let mut state: GameplayCoreState = deadsync_gameplay::init_gameplay_runtime(
        song,
        [chart.clone(), chart],
        [payload.clone(), payload],
        GameplayViewport::default(),
        GameplaySession {
            play_style,
            ..GameplaySession::default()
        },
        GameplayConfig::default(),
        deadsync_chart::SyncPref::Default,
        GameplayMiniIndicatorData::default(),
        GameplayNoteskinData::default(),
        deadsync_assets::gameplay_stage::PreparedGameplaySongLua::default(),
        deadsync_gameplay::empty_crossover_annotations,
        0,
        1.0,
        [ScrollSpeedSetting::default(); MAX_PLAYERS],
        profiles,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        [CourseLifeConfig::Bar; MAX_PLAYERS],
        false,
        [0; MAX_PLAYERS],
    );
    // Past the end of the chart, so every resolved row may be finalized.
    state.clock.song_position.current_music_time_ns = 30_000_000_000;
    state
}

pub(crate) fn state() -> GameplayCoreState {
    state_with(GameplayInputPlayStyle::Single, |_| {})
}

/// Judge a note the way gameplay does, one frame after the previous one.
fn judge(
    gs: &mut GameplayCoreState,
    note: usize,
    grade: JudgeGrade,
    window: TimingWindow,
    ms: f32,
) {
    gs.boundary.total_elapsed_in_screen += 0.001;
    gs.set_final_note_result(
        note,
        Judgment {
            time_error_ms: ms,
            time_error_music_ns: (f64::from(ms) * 1e6) as i64,
            grade,
            window: Some(window),
            miss_because_held: false,
        },
    );
    gs.update_judged_rows();
}

fn judge_first_four(gs: &mut GameplayCoreState) {
    judge(gs, 0, JudgeGrade::Fantastic, TimingWindow::W0, 5.0);
    judge(gs, 1, JudgeGrade::Fantastic, TimingWindow::W1, -18.0);
    judge(gs, 2, JudgeGrade::Excellent, TimingWindow::W2, -30.0);
    judge(gs, 3, JudgeGrade::Great, TimingWindow::W3, 60.0);
}

fn json(snapshot: &Snapshot) -> Value {
    let mut bytes = Vec::new();
    encode(snapshot, &[None, None], &mut bytes);
    serde_json::from_slice(&bytes).expect("a snapshot encodes as JSON")
}

fn close(value: &Value, expected: f64) {
    let value = value.as_f64().expect("a number");
    assert!((value - expected).abs() < 1e-4, "{value} != {expected}");
}

#[test]
fn a_snapshot_reports_what_the_hud_computes() {
    let mut gs = state();
    let mut play = Play::new("1700000000000-1");
    judge_first_four(&mut gs);
    {
        let player = &mut gs.players_runtime.players[0];
        player.holds_held = 1;
        player.mines_hit = 1;
        player.mines_avoided = 2;
    }
    gs.clock.song_position.current_music_time_ns = 12_500_000_000;
    gs.clock.song_position.current_beat_display = 31.25;
    gs.clock.song_position.current_bpm_display = 150.0;

    let message = json(
        &play
            .observe(Instant::now(), &gs)
            .expect("first frame is sent"),
    );

    assert_eq!(message["schema"], 1);
    assert_eq!(message["type"], "live");
    assert_eq!(message["play_id"], "1700000000000-1");
    assert_eq!(message["seq"], 1);
    assert_eq!(message["ended"], false);
    assert!(message["wall_us"].as_i64().unwrap() > 1_600_000_000_000_000);
    close(&message["music_time"], 12.5);
    close(
        &message["song_length"],
        f64::from(gs.song().music_length_seconds),
    );
    close(&message["music_rate"], 1.0);
    close(&message["beat"], 31.25);
    close(&message["bpm"], 150.0);

    assert_eq!(message["players"].as_array().unwrap().len(), 1);
    let player = &message["players"][0];
    assert_eq!(player["side"], "P1");
    assert_eq!(player["score_mode"], "normal");
    assert_eq!(player["show_ex_score"], false);
    let blue = gs.player_blue_window_ms(0);
    let ex = gs.display_scored_ex_score_data(0, blue);
    let itg = gs.display_gameplay_itg_score_percent(0, GameplayScoreDisplayMode::Normal);
    assert!(itg > 0.0 && itg < 100.0, "{itg}");
    close(&player["itg_percent"], itg);
    close(
        &player["ex_percent"],
        deadsync_gameplay::display_ex_score_percent_for_mode(&ex, GameplayScoreDisplayMode::Normal),
    );
    close(
        &player["hard_ex_percent"],
        deadsync_gameplay::display_hard_ex_score_percent_for_mode(
            &ex,
            GameplayScoreDisplayMode::Normal,
        ),
    );
    assert_eq!(player["combo"], 4);
    assert_eq!(player["max_combo"], 4);
    assert_eq!(player["miss_combo"], 0);
    assert_eq!(player["full_combo_grade"], "Great");
    close(&player["life"], f64::from(gs.players()[0].life));
    assert_eq!(player["failing"], false);
    assert_eq!(player["failed"], false);
    assert_eq!(
        player["judgments"],
        serde_json::json!({"fantastic": 2, "excellent": 1, "great": 1,
                           "decent": 0, "way_off": 0, "miss": 0})
    );
    close(&player["fa_plus"]["blue_window_ms"], f64::from(blue));
    for (key, count) in [
        ("w0", 1),
        ("w1", 1),
        ("w2", 1),
        ("w3", 1),
        ("w4", 0),
        ("miss", 0),
    ] {
        assert_eq!(player["fa_plus"][key], count, "{key}");
    }
    assert_eq!(player["holds"], serde_json::json!({"held": 1, "total": 1}));
    assert_eq!(player["rolls"], serde_json::json!({"held": 0, "total": 0}));
    assert_eq!(
        player["mines"],
        serde_json::json!({"hit": 1, "avoided": 2, "total": 1})
    );
    assert_eq!(player["timing"]["early"], 2);
    assert_eq!(player["timing"]["late"], 2);
    close(&player["timing"]["mean_ms"], 4.25);
    close(&player["timing"]["mean_abs_ms"], 28.25);
    close(&player["timing"]["max_abs_ms"], 60.0);
    close(&player["timing"]["recent_mean_ms"], 4.25);
    assert_eq!(
        player["last_judgment"],
        serde_json::json!({"seq": 1, "grade": "Great", "window": "W3", "blue": false,
                           "offset_ms": 60.0, "miss_because_held": false})
    );
}

#[test]
fn the_players_score_mode_is_the_one_reported() {
    let mut gs = state_with(GameplayInputPlayStyle::Single, |profile| {
        profile.score_display_mode = ScoreDisplayMode::Predictive;
        profile.show_ex_score = true;
    });
    let mut play = Play::new("a");
    judge_first_four(&mut gs);

    let message = json(&play.observe(Instant::now(), &gs).unwrap());
    let player = &message["players"][0];

    assert_eq!(player["score_mode"], "predictive");
    assert_eq!(player["show_ex_score"], true);
    let predictive = gs.display_gameplay_itg_score_percent(0, GameplayScoreDisplayMode::Predictive);
    assert_ne!(
        predictive,
        gs.display_gameplay_itg_score_percent(0, GameplayScoreDisplayMode::Normal)
    );
    close(&player["itg_percent"], predictive);
}

#[test]
fn a_blue_fantastic_and_a_miss_are_described() {
    let mut gs = state();
    let mut play = Play::new("a");
    let start = Instant::now();
    judge(&mut gs, 0, JudgeGrade::Fantastic, TimingWindow::W0, -3.0);
    let first = json(&play.observe(start, &gs).unwrap());
    assert_eq!(first["players"][0]["last_judgment"]["blue"], true);

    judge(&mut gs, 1, JudgeGrade::Miss, TimingWindow::W5, 0.0);
    let second = json(
        &play
            .observe(start + Duration::from_millis(20), &gs)
            .unwrap(),
    );
    let player = &second["players"][0];
    assert_eq!(player["last_judgment"]["seq"], 2);
    assert_eq!(player["last_judgment"]["grade"], "Miss");
    assert!(player["last_judgment"]["offset_ms"].is_null());
    assert!(player["last_judgment"]["window"].is_null());
    assert_eq!(player["full_combo_grade"], Value::Null);
    assert_eq!(
        (&player["timing"]["early"], &player["timing"]["late"]),
        (&1.into(), &0.into())
    );
}

#[test]
fn periodic_snapshots_run_at_twenty_hertz() {
    let gs = state();
    let mut play = Play::new("a");
    let start = Instant::now();
    // 1000 frames a second, and 144, both for one second.
    for frame_us in [1_000u64, 6_944] {
        let begin = start + Duration::from_secs(frame_us);
        let sent: Vec<u64> = (0..1_000_000 / frame_us)
            .filter_map(|frame| play.observe(begin + Duration::from_micros(frame * frame_us), &gs))
            .map(|snapshot| json(&snapshot)["seq"].as_u64().unwrap())
            .collect();
        assert!(
            (19..=21).contains(&sent.len()),
            "{frame_us}: {}",
            sent.len()
        );
        assert!(sent.windows(2).all(|pair| pair[1] == pair[0] + 1));
    }
}

#[test]
fn a_judgment_is_sent_without_waiting_for_the_next_period() {
    let mut gs = state();
    let mut play = Play::new("a");
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    assert!(play.observe(at(0), &gs).is_some());
    assert!(play.observe(at(10), &gs).is_none());

    judge(&mut gs, 0, JudgeGrade::Great, TimingWindow::W3, 50.0);
    let sent = json(&play.observe(at(11), &gs).expect("sent at once"));
    assert_eq!(sent["players"][0]["last_judgment"]["seq"], 1);

    judge(&mut gs, 1, JudgeGrade::Great, TimingWindow::W3, 50.0);
    assert!(
        play.observe(at(12), &gs).is_none(),
        "too close to the last one"
    );
    let sent = json(&play.observe(at(17), &gs).expect("sent once the gap passed"));
    assert_eq!(sent["players"][0]["last_judgment"]["seq"], 2);
    assert!(play.observe(at(18), &gs).is_none(), "nothing new");
}

#[test]
fn a_refused_snapshot_is_offered_again_on_the_next_frame() {
    let gs = state();
    let mut play = Play::new("a");
    let start = Instant::now();
    assert!(play.observe(start, &gs).is_some());
    play.retry();
    assert!(
        play.observe(start + Duration::from_millis(6), &gs)
            .is_some()
    );
    assert!(
        play.observe(start + Duration::from_millis(12), &gs)
            .is_none()
    );
}

#[test]
fn the_final_snapshot_is_marked_ended_and_complete() {
    let mut gs = state();
    let mut play = Play::new("a");
    assert!(play.observe(Instant::now(), &gs).is_some());
    judge_first_four(&mut gs);

    let message = json(&play.finish(&gs));

    assert_eq!(message["ended"], true);
    assert_eq!(message["seq"], 2);
    assert_eq!(message["players"][0]["judgments"]["fantastic"], 2);
    assert_eq!(message["players"][0]["timing"]["late"], 2);
}

#[test]
fn versus_reports_each_player() {
    let mut gs = state_with(GameplayInputPlayStyle::Versus, |_| {});
    let mut play = Play::new("a");
    let second_players_first_note = gs.note_range_for_player(1).0;
    judge(
        &mut gs,
        second_players_first_note,
        JudgeGrade::Excellent,
        TimingWindow::W2,
        -25.0,
    );

    let message = json(&play.observe(Instant::now(), &gs).unwrap());
    let players = message["players"].as_array().unwrap();

    assert_eq!(players.len(), 2);
    assert_eq!(
        (&players[0]["side"], &players[1]["side"]),
        (&"P1".into(), &"P2".into())
    );
    assert_eq!(players[0]["last_judgment"], Value::Null);
    assert_eq!(players[0]["timing"]["early"], 0);
    assert_eq!(players[1]["judgments"]["excellent"], 1);
    assert_eq!(players[1]["timing"]["early"], 1);
}

#[test]
fn posting_never_waits_for_the_worker() {
    let gs = state();
    let snapshot = Play::new("a").finish(&gs);
    let mailbox = Arc::new(Mailbox::default());
    assert!(mailbox.post(&snapshot));
    let held = mailbox.slot.lock().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let other = Arc::clone(&mailbox);
    std::thread::spawn(move || sender.send(other.post(&snapshot)));
    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(false));
    drop(held);
}

#[test]
fn the_port_comes_from_the_recorder_status_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(port_from_status(dir.path()), 8735);
    let file = dir.path().join("recorder-status.json");
    for (body, port) in [
        ("{\"schema\":1,\"live_port\":9999}", 9999),
        ("{\"schema\":1}", 8735),
        ("{\"live_port\":0}", 8735),
        ("{\"live_port\":70000}", 8735),
        ("not json", 8735),
    ] {
        std::fs::write(&file, body).unwrap();
        assert_eq!(port_from_status(dir.path()), port, "{body}");
    }
}

/// A play-events directory whose status file points at a local test socket.
pub(crate) fn listener() -> (tempfile::TempDir, UdpSocket) {
    let dir = tempfile::tempdir().unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    std::fs::write(
        dir.path().join("recorder-status.json"),
        format!("{{\"live_port\":{}}}", socket.local_addr().unwrap().port()),
    )
    .unwrap();
    (dir, socket)
}

pub(crate) fn receive(socket: &UdpSocket) -> Value {
    let mut buffer = [0; 65_536];
    let length = socket.recv(&mut buffer).expect("a datagram");
    serde_json::from_slice(&buffer[..length]).unwrap()
}

#[test]
fn the_publisher_sends_datagrams_from_its_worker() {
    let (dir, socket) = listener();
    let mut gs = state();
    let mut publisher = Publisher::default();
    publisher.start(dir.path(), "play-1", 0);

    publisher.frame(Instant::now(), &gs);
    let first = receive(&socket);
    assert_eq!(
        (&first["play_id"], &first["ended"]),
        (&"play-1".into(), &false.into())
    );

    judge_first_four(&mut gs);
    publisher.finish(&gs);
    let last = receive(&socket);
    assert_eq!((&last["seq"], &last["ended"]), (&2.into(), &true.into()));

    // A finished play sends nothing more; the next play starts its own count.
    publisher.frame(Instant::now() + Duration::from_secs(1), &gs);
    publisher.finish(&gs);
    publisher.start(dir.path(), "play-2", 0);
    publisher.frame(Instant::now(), &gs);
    let next = receive(&socket);
    assert_eq!(
        (&next["play_id"], &next["seq"]),
        (&"play-2".into(), &1.into())
    );
    drop(publisher);
}

#[test]
fn an_idle_publisher_has_no_worker() {
    let gs = state();
    let mut publisher = Publisher::default();
    publisher.frame(Instant::now(), &gs);
    publisher.finish(&gs);
    assert!(publisher.worker.is_none());
}

/// Waking a thread costs a system call, so frames must not have to.
#[test]
fn a_snapshot_posted_during_a_play_is_sent_without_waking_the_worker() {
    let (dir, socket) = listener();
    let gs = state();
    let mut publisher = Publisher::default();
    publisher.start(dir.path(), "play-1", 0);
    // Let the worker settle into its wait.
    std::thread::sleep(Duration::from_millis(50));

    let snapshot = Play::new("play-1").finish(&gs);
    assert!(publisher.worker.as_ref().unwrap().mailbox.post(&snapshot));

    assert_eq!(receive(&socket)["play_id"], "play-1");
}

/// A worker that locked an empty mailbox could make a frame's post fail.
#[test]
fn the_worker_leaves_an_empty_mailbox_unlocked() {
    let mailbox = Arc::new(Mailbox::default());
    let held = mailbox.slot.lock().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let other = Arc::clone(&mailbox);
    std::thread::spawn(move || sender.send(other.take().is_none()));
    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(true));
    drop(held);
}

#[test]
fn snapshots_list_the_pads_in_use() {
    let mut gs = state();
    let mut play = Play::new("a");
    gs.control
        .input_state
        .update_slot(1, deadsync_core::input::InputSource::Keyboard, 0, true);

    let message = json(&play.observe(Instant::now(), &gs).unwrap());

    let pads = message["pads"].as_array().unwrap();
    assert_eq!(pads.len(), 1);
    assert_eq!(pads[0]["side"], "P1");
    assert_eq!(pads[0]["player"], 0);
    assert_eq!(pads[0]["source"], "input");
    let down: Vec<bool> = (0..9).map(|panel| panel == 7).collect();
    assert_eq!(pads[0]["pressed"], serde_json::json!(down));
    assert_eq!(pads[0]["pressure"], Value::Null);
}

#[test]
fn a_panel_press_is_sent_without_waiting_for_the_next_period() {
    let mut gs = state();
    let mut play = Play::new("a");
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    assert!(play.observe(at(0), &gs).is_some());
    assert!(play.observe(at(10), &gs).is_none());

    gs.control
        .input_state
        .update_slot(0, deadsync_core::input::InputSource::Keyboard, 0, true);
    let sent = json(&play.observe(at(11), &gs).expect("sent at once"));
    assert_eq!(sent["pads"][0]["pressed"][3], true);
    assert!(play.observe(at(20), &gs).is_none(), "nothing new");

    // An SMX panel the game has no lane for counts too.
    play.smx = |_| 1 << 4;
    assert!(play.observe(at(21), &gs).is_some());
}

/// A test-mode switch that records which thread called it.
static SENSOR_CALLS: Mutex<Vec<(usize, Option<String>)>> = Mutex::new(Vec::new());

fn record_sensor_call(slot: usize) {
    let thread = std::thread::current().name().map(str::to_owned);
    SENSOR_CALLS.lock().unwrap().push((slot, thread));
}

#[test]
fn a_pressure_request_is_applied_once_by_the_worker() {
    let gate = PressureGate::default();
    let mut applied = 0;
    let mut calls = Vec::new();
    gate.apply(&mut applied, |slot| calls.push(slot));
    assert!(calls.is_empty(), "nothing asked for yet");

    gate.request(1, 0b11);
    gate.apply(&mut applied, |slot| calls.push(slot));
    gate.apply(&mut applied, |slot| calls.push(slot));
    assert_eq!(calls, [0, 1]);

    // A play without pressure turns nothing on; the next one with it does.
    gate.request(2, 0);
    gate.apply(&mut applied, |slot| calls.push(slot));
    gate.request(3, 0b10);
    gate.apply(&mut applied, |slot| calls.push(slot));
    assert_eq!(calls, [0, 1, 1]);
}

/// Leaving gameplay turns test mode off on the game thread. A request the
/// worker had not reached by then must never turn it back on.
#[test]
fn a_request_from_a_play_that_has_ended_is_dropped() {
    let gate = PressureGate::default();
    let mut applied = 0;
    let mut calls = Vec::new();
    gate.request(1, 0b01);
    gate.end(1);
    gate.apply(&mut applied, |slot| calls.push(slot));
    assert!(calls.is_empty());

    // Ending a later play covers any older request still waiting.
    gate.request(2, 0b01);
    gate.request(3, 0);
    gate.end(3);
    gate.apply(&mut applied, |slot| calls.push(slot));
    assert!(calls.is_empty());
}

#[test]
fn ending_a_play_waits_for_a_test_mode_call_in_flight() {
    let gate = Arc::new(PressureGate::default());
    gate.request(1, 0b01);
    let (entered, in_call) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let worker = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            let mut applied = 0;
            gate.apply(&mut applied, |_| {
                entered.send(()).unwrap();
                released.recv().unwrap();
            });
        })
    };
    in_call.recv_timeout(Duration::from_secs(2)).unwrap();
    let (ended, has_ended) = std::sync::mpsc::channel();
    let ender = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            gate.end(1);
            ended.send(()).unwrap();
        })
    };
    assert!(
        has_ended.recv_timeout(Duration::from_millis(100)).is_err(),
        "the end of the play must wait for the call"
    );
    release.send(()).unwrap();
    has_ended.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    ender.join().unwrap();
}

#[test]
fn the_worker_turns_on_test_mode_only_when_a_play_asks() {
    let (dir, _socket) = listener();
    let gs = state();
    let mut publisher = Publisher::default();
    publisher.sensor_on = record_sensor_call;
    publisher.start(dir.path(), "play-1", 0);
    publisher.frame(Instant::now(), &gs);
    publisher.finish(&gs);
    std::thread::sleep(Duration::from_millis(50));
    assert!(SENSOR_CALLS.lock().unwrap().is_empty());

    publisher.start(dir.path(), "play-2", 0b10);
    let deadline = Instant::now() + Duration::from_secs(2);
    while SENSOR_CALLS.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    // From the worker, never from the game thread that started the play.
    assert_eq!(
        *SENSOR_CALLS.lock().unwrap(),
        [(1, Some("play-live".to_owned()))]
    );
    publisher.finish(&gs);
    drop(publisher);
    assert_eq!(SENSOR_CALLS.lock().unwrap().len(), 1);
}
