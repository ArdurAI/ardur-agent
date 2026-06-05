//! §2.X typing-dots animation (design §D).
//!
//! While a turn waits for its first content token, the REPL shows `·`/`··`/`···`
//! cycling at 4 Hz on a single line, then erases it with a carriage return +
//! erase-to-end-of-line the instant the first delta lands. The animation is
//! data-only here (frames + cadence + clear sequence); the REPL drives it with a
//! [`tokio::time::interval`] and auto-disables it on a non-tty / `NO_COLOR`.

use std::time::Duration;

use crate::theme::{Role, Theme};

/// The animation rate: 4 frames per second (design §D).
pub const TYPING_DOTS_HZ: u32 = 4;

/// The tick interval implied by [`TYPING_DOTS_HZ`] — 250 ms.
pub const TYPING_DOTS_TICK: Duration = Duration::from_millis(1000 / TYPING_DOTS_HZ as u64);

/// The three pulsing frames, in cycle order.
pub const TYPING_DOTS_FRAMES: [&str; 3] = ["·", "··", "···"];

/// The escape that erases the dots line before the first token prints: carriage
/// return + erase-to-end-of-line.
pub const CLEAR_LINE: &str = "\r\x1b[2K";

/// A cycling typing-dots animator. Advance one frame per [`TYPING_DOTS_TICK`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TypingDots {
    frame: usize,
}

impl TypingDots {
    /// A fresh animator at the first frame.
    #[must_use]
    pub fn new() -> Self {
        Self { frame: 0 }
    }

    /// The current frame's glyphs, painted dim through `theme`.
    #[must_use]
    pub fn render(&self, theme: &Theme) -> String {
        theme.paint(
            Role::Dim,
            TYPING_DOTS_FRAMES[self.frame % TYPING_DOTS_FRAMES.len()],
        )
    }

    /// Advance to the next frame (wrapping).
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % TYPING_DOTS_FRAMES.len();
    }

    /// The current frame index (0-based).
    #[must_use]
    pub fn frame(&self) -> usize {
        self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_is_four_hz_three_frames() {
        assert_eq!(TYPING_DOTS_HZ, 4);
        assert_eq!(TYPING_DOTS_TICK, Duration::from_millis(250));
        assert_eq!(TYPING_DOTS_FRAMES.len(), 3);
    }

    #[test]
    fn frames_cycle_in_order() {
        let mut dots = TypingDots::new();
        assert_eq!(dots.frame(), 0);
        dots.tick();
        assert_eq!(dots.frame(), 1);
        dots.tick();
        assert_eq!(dots.frame(), 2);
        dots.tick();
        assert_eq!(dots.frame(), 0); // wraps
    }
}
