//! Transient on-screen readout of the system volume (shown when media keys
//! change it). Pure presentation: the shell decides when it is visible.

use crate::act;
use deadlib_present::actors::Actor;
use std::sync::Arc;

const Z: i16 = 32010;
const BOX_W: f32 = 260.0;
const BOX_H: f32 = 52.0;
const BAR_W: f32 = 220.0;
const BAR_H: f32 = 6.0;
/// Distance of the box centre from the bottom edge of the screen.
const BOTTOM_MARGIN: f32 = 70.0;

/// Seconds the readout stays fully visible after the last change.
pub const SHOW_SECONDS: f32 = 1.4;
/// Seconds spent fading out at the end of the visible window.
pub const FADE_SECONDS: f32 = 0.3;

/// Opacity multiplier `seconds` after the last volume change: 1 while
/// visible, linear fade over the final [`FADE_SECONDS`], 0 afterwards.
#[must_use]
pub fn visibility_alpha(seconds_since_change: f32) -> f32 {
    if !seconds_since_change.is_finite() || seconds_since_change < 0.0 {
        return 0.0;
    }
    let fade_start = SHOW_SECONDS - FADE_SECONDS;
    if seconds_since_change <= fade_start {
        1.0
    } else if seconds_since_change >= SHOW_SECONDS {
        0.0
    } else {
        1.0 - (seconds_since_change - fade_start) / FADE_SECONDS
    }
}

/// Fraction of the bar to fill for `percent` (the bar tops out at 100%).
#[must_use]
pub fn fill_fraction(percent: u16, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        f32::from(percent.min(100)) / 100.0
    }
}

/// Label text for the readout.
#[must_use]
pub fn label(percent: u16, muted: bool) -> String {
    if muted {
        "Muted".to_owned()
    } else {
        format!("Volume {percent}%")
    }
}

/// Build the readout centred near the bottom of the screen.
#[must_use]
pub fn build(percent: u16, muted: bool, alpha: f32, screen_w: f32, screen_h: f32) -> Vec<Actor> {
    let alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return Vec::new();
    }
    let cx = screen_w * 0.5;
    let cy = screen_h - BOTTOM_MARGIN;
    let bar_left = cx - BAR_W * 0.5;
    let bar_y = cy + 12.0;
    let fill_w = BAR_W * fill_fraction(percent, muted);
    // Above 100% (software boost) the fill turns orange as a warning.
    let fill = if percent > 100 {
        [1.0, 0.6, 0.2]
    } else {
        [0.3, 0.85, 1.0]
    };
    let text: Arc<str> = Arc::from(label(percent, muted));

    let mut actors = Vec::with_capacity(4);
    actors.push(act!(quad:
        align(0.5, 0.5):
        xy(cx, cy):
        zoomto(BOX_W, BOX_H):
        diffuse(0.0, 0.0, 0.0, 0.8 * alpha):
        z(Z)
    ));
    actors.push(act!(text:
        font("miso"):
        settext(text):
        align(0.5, 0.5):
        xy(cx, cy - 10.0):
        zoom(0.9):
        diffuse(1.0, 1.0, 1.0, alpha):
        horizalign(center):
        z(Z + 1)
    ));
    actors.push(act!(quad:
        align(0.0, 0.5):
        xy(bar_left, bar_y):
        zoomto(BAR_W, BAR_H):
        diffuse(1.0, 1.0, 1.0, 0.25 * alpha):
        z(Z + 1)
    ));
    if fill_w > 0.0 {
        actors.push(act!(quad:
            align(0.0, 0.5):
            xy(bar_left, bar_y):
            zoomto(fill_w, BAR_H):
            diffuse(fill[0], fill[1], fill[2], alpha):
            z(Z + 2)
        ));
    }
    actors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_is_full_then_fades_then_zero() {
        assert_eq!(visibility_alpha(0.0), 1.0);
        assert_eq!(visibility_alpha(SHOW_SECONDS - FADE_SECONDS), 1.0);
        let mid = visibility_alpha(SHOW_SECONDS - FADE_SECONDS * 0.5);
        assert!((mid - 0.5).abs() < 1e-5, "mid-fade alpha was {mid}");
        assert_eq!(visibility_alpha(SHOW_SECONDS), 0.0);
        assert_eq!(visibility_alpha(60.0), 0.0);
        assert_eq!(visibility_alpha(-1.0), 0.0);
        assert_eq!(visibility_alpha(f32::NAN), 0.0);
    }

    #[test]
    fn fill_and_label_follow_level_and_mute() {
        assert_eq!(fill_fraction(0, false), 0.0);
        assert_eq!(fill_fraction(50, false), 0.5);
        assert_eq!(fill_fraction(130, false), 1.0);
        assert_eq!(fill_fraction(80, true), 0.0);
        assert_eq!(label(70, false), "Volume 70%");
        assert_eq!(label(70, true), "Muted");
    }

    #[test]
    fn build_is_empty_when_invisible_and_populated_when_shown() {
        assert!(build(50, false, 0.0, 640.0, 480.0).is_empty());
        assert_eq!(build(50, false, 1.0, 640.0, 480.0).len(), 4);
        // Muted or zero level has no fill actor.
        assert_eq!(build(50, true, 1.0, 640.0, 480.0).len(), 3);
        assert_eq!(build(0, false, 1.0, 640.0, 480.0).len(), 3);
    }
}
