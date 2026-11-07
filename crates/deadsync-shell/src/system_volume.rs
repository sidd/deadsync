//! Follows the system (sound server) output volume so the game can show an
//! on-screen level when it changes, e.g. from the media keys.
//!
//! On the dance box the media keys are handled by the window manager, which
//! calls `pactl` on the default sink; the game never sees those key presses.
//! A background thread therefore runs `pactl subscribe` and re-reads the
//! default sink's volume/mute when the server reports a change.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

use deadlib_present::actors::Actor;
use deadsync_theme_simply_love::screens::components::shared::volume_indicator;

/// A snapshot of the default output sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeReading {
    pub percent: u16,
    pub muted: bool,
}

/// First `NN%` token in `pactl get-sink-volume` output.
#[must_use]
pub fn parse_volume_percent(output: &str) -> Option<u16> {
    output.split_whitespace().find_map(|token| {
        token
            .strip_suffix('%')
            .and_then(|digits| digits.parse::<u16>().ok())
    })
}

/// `pactl get-sink-mute` prints `Mute: yes` / `Mute: no`.
#[must_use]
pub fn parse_mute(output: &str) -> Option<bool> {
    let value = output.split(':').nth(1)?.trim();
    match value {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// True for a `pactl subscribe` line that may change the default sink's level.
#[must_use]
pub fn is_relevant_event(line: &str) -> bool {
    line.contains(" on sink #") || line.contains(" on server")
}

/// Decides when the readout is visible. The first reading only records the
/// current level; later readings that differ restart the visible window.
#[derive(Debug, Default)]
pub struct VolumeIndicatorState {
    last: Option<VolumeReading>,
    changed_at: Option<Instant>,
}

impl VolumeIndicatorState {
    pub fn observe(&mut self, reading: VolumeReading, now: Instant) {
        if self.last.is_some_and(|last| last != reading) {
            self.changed_at = Some(now);
        }
        self.last = Some(reading);
    }

    /// The reading to draw and its opacity, if the readout is visible at `now`.
    #[must_use]
    pub fn visible(&self, now: Instant) -> Option<(VolumeReading, f32)> {
        let reading = self.last?;
        let changed_at = self.changed_at?;
        let alpha = volume_indicator::visibility_alpha(
            now.saturating_duration_since(changed_at).as_secs_f32(),
        );
        (alpha > 0.0).then_some((reading, alpha))
    }
}

/// Owns the watcher thread's channel and the visibility state.
pub struct SystemVolumeIndicator {
    receiver: Option<Receiver<VolumeReading>>,
    state: VolumeIndicatorState,
}

impl SystemVolumeIndicator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            receiver: spawn_watcher(),
            state: VolumeIndicatorState::default(),
        }
    }

    /// Drain pending readings and append the readout if it is visible.
    /// `enabled` (the user setting) is only evaluated while a readout would
    /// otherwise be drawn, so idle frames never touch the config.
    pub fn append_actors(
        &mut self,
        enabled: impl FnOnce() -> bool,
        actors: &mut Vec<Actor>,
        now: Instant,
        screen_w: f32,
        screen_h: f32,
    ) {
        if let Some(receiver) = &self.receiver {
            while let Ok(reading) = receiver.try_recv() {
                self.state.observe(reading, now);
            }
        }
        if let Some((reading, alpha)) = self.state.visible(now)
            && enabled()
        {
            actors.extend(volume_indicator::build(
                reading.percent,
                reading.muted,
                alpha,
                screen_w,
                screen_h,
            ));
        }
    }
}

impl Default for SystemVolumeIndicator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
fn spawn_watcher() -> Option<Receiver<VolumeReading>> {
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new()
        .name("system-volume".to_owned())
        .spawn(move || watch_default_sink(&tx));
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            log::warn!("Could not start the system volume watcher: {e}");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn spawn_watcher() -> Option<Receiver<VolumeReading>> {
    None
}

#[cfg(target_os = "linux")]
fn query_default_sink() -> Option<VolumeReading> {
    use std::process::Command;
    let volume = Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    let mute = Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    Some(VolumeReading {
        percent: parse_volume_percent(&String::from_utf8_lossy(&volume.stdout))?,
        muted: parse_mute(&String::from_utf8_lossy(&mute.stdout))?,
    })
}

#[cfg(target_os = "linux")]
fn watch_default_sink(tx: &Sender<VolumeReading>) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    // No pactl (or no sound server): the indicator simply never shows.
    let Some(initial) = query_default_sink() else {
        log::info!("System volume indicator unavailable (pactl could not read the default sink).");
        return;
    };
    if tx.send(initial).is_err() {
        return;
    }
    let mut last_sent = initial;
    let child = Command::new("pactl")
        .arg("subscribe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            break;
        };
        if !is_relevant_event(&line) {
            continue;
        }
        let Some(reading) = query_default_sink() else {
            continue;
        };
        if reading != last_sent {
            last_sent = reading;
            if tx.send(reading).is_err() {
                // The game is shutting down; stop the subscriber too.
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_pactl_volume_and_mute_output() {
        let volume = "Volume: front-left: 52428 /  80% / -5.81 dB,   front-right: 52428 /  80% / -5.81 dB\n        balance 0.00\n";
        assert_eq!(parse_volume_percent(volume), Some(80));
        assert_eq!(parse_volume_percent("Volume: mono: 65536 / 100% / 0.00 dB"), Some(100));
        assert_eq!(parse_volume_percent("Volume: front-left: 0 /   0% / -inf dB"), Some(0));
        assert_eq!(parse_volume_percent("no percentages here"), None);
        assert_eq!(parse_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_mute("garbage"), None);
    }

    #[test]
    fn only_sink_and_server_events_are_relevant() {
        assert!(is_relevant_event("Event 'change' on sink #51"));
        assert!(is_relevant_event("Event 'change' on server #0"));
        assert!(!is_relevant_event("Event 'change' on source #52"));
        assert!(!is_relevant_event("Event 'new' on sink-input #12"));
    }

    #[test]
    fn first_reading_is_silent_and_changes_show_the_readout() {
        let t0 = Instant::now();
        let mut state = VolumeIndicatorState::default();
        let a = VolumeReading { percent: 80, muted: false };

        state.observe(a, t0);
        assert!(state.visible(t0).is_none(), "initial level must not flash");

        state.observe(a, t0 + Duration::from_secs(1));
        assert!(state.visible(t0 + Duration::from_secs(1)).is_none());

        let b = VolumeReading { percent: 90, muted: false };
        state.observe(b, t0 + Duration::from_secs(2));
        let (reading, alpha) = state.visible(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(reading, b);
        assert_eq!(alpha, 1.0);
        assert!(state.visible(t0 + Duration::from_secs(2) + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn mute_toggle_counts_as_a_change_and_repeats_restart_the_window() {
        let t0 = Instant::now();
        let mut state = VolumeIndicatorState::default();
        state.observe(VolumeReading { percent: 50, muted: false }, t0);
        let muted = VolumeReading { percent: 50, muted: true };
        state.observe(muted, t0 + Duration::from_millis(100));
        assert_eq!(state.visible(t0 + Duration::from_millis(100)).unwrap().0, muted);

        // A later change near the end of the window keeps it up.
        let later = t0 + Duration::from_millis(1300);
        state.observe(VolumeReading { percent: 60, muted: false }, later);
        let (_, alpha) = state.visible(later + Duration::from_millis(100)).unwrap();
        assert_eq!(alpha, 1.0);
    }
}
