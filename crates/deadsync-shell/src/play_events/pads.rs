//! sidd fork: the panels of each pad in use, for the live snapshots.
//!
//! Every frame the game thread copies two bitmasks per pad: the panels the
//! game sees as held (its lane state) and the panels the `StepManiaX` pad in
//! that slot reports, a relaxed atomic load each. A change makes the next
//! snapshot go out at once. The worker chooses which mask to report and reads
//! the sensor values for pressure itself, so the game thread never touches the
//! SDK during a song.
//!
//! Pressure needs the SDK's sensor test mode, which streams sensor values over
//! USB at about 30 Hz per pad. The gameplay FSR display option turns it on for
//! its own pads, and the recorder can ask for it with `pad_pressure` in its
//! status file; the live worker then turns it on (see `start_sensor_test`).
//! Test mode only makes values available: the on-screen display is drawn from
//! views that gameplay fills for the player's own option, never for the
//! recorder. Leaving gameplay turns test mode off again.

use deadsync_assets::gameplay_stage::GameplayCoreState;
use deadsync_core::input::{LaneMask, MAX_PLAYERS};
use deadsync_gameplay::{GameplayInputPlayStyle, GameplayInputPlayerSide};
use deadsync_smx::SensorTestData;
use serde::Serialize;

pub(super) const PANELS: usize = deadsync_smx::PANEL_COUNT;
const PADS: usize = 2;
/// Full scale of a calibrated sensor value, as in the gameplay FSR display.
const PRESSURE_SCALE: f32 = 250.0;
/// Panels in SDK order (UL U UR L C R DL D DR) for dance lanes L D U R.
const DANCE_PANELS: [u8; 4] = [3, 7, 1, 5];
/// Panels for pump lanes DL UL C UR DR.
const PUMP_PANELS: [u8; 5] = [6, 0, 4, 2, 8];

/// One pad in use, as copied on the game thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Pad {
    /// 0 is the P1 (left) pad, 1 the P2 pad: the SDK's pad slots.
    slot: u8,
    /// Index into the snapshot's players.
    player: u8,
    /// Panels held according to the game's own input, bit n for panel n.
    lanes: u16,
    /// Panels the SMX pad in this slot reports; 0 without one.
    smx: u16,
}

/// The pads in use. Plain data, so copying it never allocates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Pads {
    count: usize,
    pads: [Pad; PADS],
}

const fn slot_of(side: GameplayInputPlayerSide) -> usize {
    match side {
        GameplayInputPlayerSide::P1 => 0,
        GameplayInputPlayerSide::P2 => 1,
    }
}

/// `(slot, player, first lane)` of each pad in use. Double and Routine
/// play on both pads; everyone else plays on the pad of their side.
fn layout(
    style: GameplayInputPlayStyle,
    num_players: usize,
    side: impl Fn(usize) -> GameplayInputPlayerSide,
) -> ([(usize, usize, usize); PADS], usize) {
    let panels = if style.is_pump() {
        PUMP_PANELS.len()
    } else {
        DANCE_PANELS.len()
    };
    let mut pads = [(0, 0, 0); PADS];
    if style.is_double() || style.is_routine() {
        return ([(0, 0, 0), (1, 0, panels)], PADS);
    }
    let count = num_players.min(MAX_PLAYERS).min(PADS);
    for (player, pad) in pads.iter_mut().enumerate().take(count) {
        *pad = (slot_of(side(player)), player, player * style.cols_per_player());
    }
    (pads, count)
}

/// The panels of the lanes starting at `first`.
fn lane_panels(lanes: LaneMask, first: usize, pump: bool) -> u16 {
    let panels: &[u8] = if pump { &PUMP_PANELS } else { &DANCE_PANELS };
    let mut bits = 0;
    for (lane, &panel) in panels.iter().enumerate() {
        let lane = first + lane;
        if lane < LaneMask::BITS as usize && lanes & (1 << lane) != 0 {
            bits |= 1 << panel;
        }
    }
    bits
}

impl Pads {
    /// Game thread, every frame. `smx` reads a slot's panel bits.
    pub(super) fn capture(gs: &GameplayCoreState, smx: fn(usize) -> u16) -> Self {
        let session = &gs.setup.session;
        let style = session.play_style;
        let (layout, count) = layout(style, gs.num_players(), |player| {
            session.runtime_player_side(player)
        });
        let lanes = gs.control.input_state.pressed_lane_mask();
        let mut pads = Self {
            count,
            ..Self::default()
        };
        for (pad, &(slot, player, first)) in pads.pads.iter_mut().zip(&layout).take(count) {
            *pad = Pad {
                slot: slot as u8,
                player: player as u8,
                lanes: lane_panels(lanes, first, style.is_pump()),
                smx: smx(slot),
            };
        }
        pads
    }

    fn as_slice(&self) -> &[Pad] {
        &self.pads[..self.count.min(PADS)]
    }
}

/// Lock-free panel bits of an SMX pad slot.
pub(super) fn smx_panel_bits(slot: usize) -> u16 {
    deadsync_smx::panel_bits(slot)
}

/// The pad slots of this play, bit n for slot n. Game thread, once as the
/// play starts: plain data, no SDK call.
pub(super) fn pressure_slots(gs: &GameplayCoreState) -> u8 {
    Pads::capture(gs, |_| 0)
        .as_slice()
        .iter()
        .fold(0, |slots, pad| slots | (1 << pad.slot))
}

/// Worker: ask the SMX pad in a slot for sensor values. Takes the SDK's lock,
/// which its USB thread holds during writes and device opens, so it never runs
/// on the game thread. Costs what the gameplay FSR display costs when it
/// starts; leaving gameplay turns it off again.
pub(super) fn start_sensor_test(slot: usize) {
    deadsync_smx::set_test_mode(slot, deadsync_smx::SensorTestMode::CalibratedValues);
}

/// What the worker knows about the SMX pad in a slot.
#[derive(Clone, Default)]
pub(super) struct Sensors {
    pub(super) fsr: bool,
    /// The newest sensor values, while sensor test mode is on.
    pub(super) data: Option<SensorTestData>,
}

/// Per slot; `None` where no SMX pad is connected.
pub(super) type SlotSensors = [Option<Sensors>; PADS];

/// Worker: which slots have an SMX pad, and which of those use FSRs. Takes
/// the SDK's lock, so it is looked up once per play.
pub(super) fn connected_pads() -> SlotSensors {
    std::array::from_fn(|slot| {
        deadsync_smx::get_info(slot).connected.then(|| Sensors {
            fsr: deadsync_smx::get_config(slot).is_some_and(|config| deadsync_smx::is_fsr(&config)),
            data: None,
        })
    })
}

/// Worker: refresh the sensor values of the connected pads. Uses the SMX
/// crate's own copy, never the SDK's lock.
pub(super) fn read_sensors(sensors: &mut SlotSensors) {
    for (slot, pad) in sensors.iter_mut().enumerate() {
        if let Some(pad) = pad {
            pad.data = deadsync_smx::get_test_data(slot);
        }
    }
}

#[derive(Serialize)]
pub(super) struct PadMessage {
    side: &'static str,
    player: u8,
    source: &'static str,
    pressed: [bool; PANELS],
    pressure: Option<[Option<f32>; PANELS]>,
}

fn pressure(sensors: &Sensors) -> Option<[Option<f32>; PANELS]> {
    let data = sensors.data.as_ref()?;
    Some(std::array::from_fn(|panel| {
        crate::gameplay_runtime::smx_sensor_value(data, panel, sensors.fsr).map(|value| {
            let level = (f32::from(value) / PRESSURE_SCALE).clamp(0.0, 1.0);
            (level * 100.0).round() / 100.0
        })
    }))
}

/// Worker: the pads as sent.
pub(super) fn messages(pads: &Pads, sensors: &SlotSensors) -> Vec<PadMessage> {
    pads.as_slice()
        .iter()
        .map(|pad| {
            let smx = sensors.get(usize::from(pad.slot)).and_then(Option::as_ref);
            let bits = if smx.is_some() { pad.smx } else { pad.lanes };
            PadMessage {
                side: if pad.slot == 0 { "P1" } else { "P2" },
                player: pad.player,
                source: if smx.is_some() { "smx" } else { "input" },
                pressed: std::array::from_fn(|panel| bits & (1 << panel) != 0),
                pressure: smx.and_then(pressure),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadsync_core::input::InputSource;
    use GameplayInputPlayStyle as Style;
    use GameplayInputPlayerSide::{P1, P2};

    fn press(gs: &mut GameplayCoreState, lane: usize) {
        gs.control
            .input_state
            .update_slot(lane, InputSource::Keyboard, lane as u32, true);
    }

    fn pressed(message: &PadMessage) -> Vec<usize> {
        (0..PANELS).filter(|&panel| message.pressed[panel]).collect()
    }

    #[test]
    fn each_style_uses_the_pads_of_its_sides() {
        let sides = |player| [P1, P2][player];
        assert_eq!(layout(Style::Single, 1, |_| P1).1, 1);
        assert_eq!(layout(Style::Single, 1, |_| P2).0[0], (1, 0, 0));
        assert_eq!(layout(Style::Versus, 2, sides), ([(0, 0, 0), (1, 1, 4)], 2));
        assert_eq!(layout(Style::Double, 1, |_| P2), ([(0, 0, 0), (1, 0, 4)], 2));
        assert_eq!(layout(Style::PumpDouble, 1, |_| P1).0[1], (1, 0, 5));
        assert_eq!(layout(Style::PumpVersus, 2, sides).0[1], (1, 1, 5));
    }

    #[test]
    fn lanes_become_panels() {
        // Left and Up on the first pad; Right on the second.
        let lanes = 0b1000_0101;
        assert_eq!(lane_panels(lanes, 0, false), (1 << 3) | (1 << 1));
        assert_eq!(lane_panels(lanes, 4, false), 1 << 5);
        // Pump: down-left and centre.
        assert_eq!(lane_panels(0b101, 0, true), (1 << 6) | (1 << 4));
        assert_eq!(lane_panels(u32::MAX, 30, false), (1 << 3) | (1 << 7));
    }

    #[test]
    fn without_an_smx_pad_the_games_own_input_is_reported() {
        let mut gs = super::super::live::tests::state();
        press(&mut gs, 0);
        press(&mut gs, 3);
        let pads = Pads::capture(&gs, |_| 0b1_1111_1111);

        let sent = messages(&pads, &[None, None]);

        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].side, sent[0].player), ("P1", 0));
        assert_eq!(sent[0].source, "input");
        assert_eq!(pressed(&sent[0]), [3, 5]);
        assert!(sent[0].pressure.is_none());
    }

    #[test]
    fn an_smx_pad_reports_all_nine_panels_and_pressure() {
        let gs = super::super::live::tests::state_with(Style::Versus, |_| {});
        let pads = Pads::capture(&gs, |slot| if slot == 1 { 0b1_0001_0001 } else { 0 });
        let mut data = SensorTestData::default();
        data.have_data_from_panel = [true; PANELS];
        data.have_data_from_panel[2] = false;
        data.sensor_level[0] = [400, 0, 1200, 0]; // FSR: 1200 >> 2 = 300, full
        data.sensor_level[4] = [500, 0, 0, 0]; // 125 of 250
        let sensors = [
            None,
            Some(Sensors {
                fsr: true,
                data: Some(data),
            }),
        ];

        let sent = messages(&pads, &sensors);

        assert_eq!(sent.len(), 2);
        assert_eq!((sent[0].side, sent[0].source), ("P1", "input"));
        assert_eq!((sent[1].side, sent[1].player, sent[1].source), ("P2", 1, "smx"));
        assert_eq!(pressed(&sent[1]), [0, 4, 8]);
        let pressure = sent[1].pressure.expect("sensor values");
        assert_eq!(pressure[0], Some(1.0));
        assert_eq!(pressure[4], Some(0.5));
        assert_eq!(pressure[1], Some(0.0));
        assert_eq!(pressure[2], None);
    }

    #[test]
    fn an_smx_pad_without_sensor_values_has_no_pressure() {
        let gs = super::super::live::tests::state();
        let pads = Pads::capture(&gs, |_| 1 << 4);
        let sensors = [Some(Sensors::default()), None];
        let sent = messages(&pads, &sensors);
        assert_eq!(sent[0].source, "smx");
        assert_eq!(pressed(&sent[0]), [4]);
        assert!(sent[0].pressure.is_none());
    }

    #[test]
    fn pressure_is_asked_for_the_pads_in_use() {
        assert_eq!(pressure_slots(&super::super::live::tests::state()), 0b01);
        for style in [Style::Versus, Style::Double] {
            let gs = super::super::live::tests::state_with(style, |_| {});
            assert_eq!(pressure_slots(&gs), 0b11, "{style:?}");
        }
    }

    #[test]
    fn without_the_sdk_no_pad_is_connected() {
        assert!(connected_pads().iter().all(Option::is_none));
        assert_eq!(smx_panel_bits(0), 0);
    }
}
