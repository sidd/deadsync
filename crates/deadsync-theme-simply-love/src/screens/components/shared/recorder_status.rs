//! sidd fork: external-recorder status lines under the stats overlay.

use crate::act;
use deadlib_present::actors::Actor;
use deadlib_present::space::screen_width;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Warning,
    Alert,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: Arc<str>,
    pub tone: Tone,
}

/// Draw `lines` directly under the stats FPS block. Call right after
/// `stats_overlay::push`; `stats_start` is `actors.len()` from before it.
pub fn push(actors: &mut Vec<Actor>, stats_start: usize, lines: &[Line]) {
    // Matches `stats_overlay`: its FPS block is three lines, 48 units tall,
    // and anything it draws after that block starts where these lines go.
    const MARGIN_X: f32 = -16.0;
    const TOP_Y: f32 = 16.0 + 48.0;
    const LINE_Y: f32 = 16.0;
    const Z: i16 = 32020;

    let height = LINE_Y * lines.len() as f32;
    for actor in actors.iter_mut().skip(stats_start + 1) {
        if let Actor::Text { offset, .. } = actor {
            offset[1] += height;
        }
    }
    let x = screen_width() + MARGIN_X;
    let mut y = TOP_Y;
    actors.reserve(lines.len());
    for line in lines {
        let [r, g, b] = match line.tone {
            Tone::Normal => [1.0, 1.0, 1.0],
            Tone::Warning => [1.0, 1.0, 0.0],
            Tone::Alert => [1.0, 0.3, 0.3],
        };
        actors.push(act!(text:
            align(1.0, 0.0):
            xy(x, y):
            zoom(0.65):
            diffuse(r, g, b, 1.0):
            font("miso"):
            settext(Arc::clone(&line.text)):
            horizalign(right):
            z(Z)
        ));
        y += LINE_Y;
    }
}
