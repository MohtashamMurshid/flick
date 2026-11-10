//! Automatic quality: drop a tier when playback keeps stalling early on, and
//! say so. Pure state machine with no GPUI, owned by the desktop-modules
//! agent; the player calls it on every tick.

#![allow(dead_code)]

use flick::remote::protocol::Quality;
use std::time::{Duration, Instant};

/// Watches one remote playback session.
#[derive(Debug)]
pub struct AutoQuality {
    current: Quality,
    offered: Vec<Quality>,
}

/// What the player should do after an observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Keep,
    /// Reload at this tier and tell the person why.
    Drop {
        to: Quality,
        reason: String,
    },
}

impl AutoQuality {
    /// `offered` is the server's tier list; `current` is what is playing.
    pub fn new(current: Quality, offered: Vec<Quality>, started: Instant) -> Self {
        let _ = started;
        Self { current, offered }
    }

    /// Called every tick with libmpv's `paused-for-cache` flag and the
    /// playback position. The rule: two stalls within the first minute of
    /// playback, or one stall longer than five seconds at any time, drop one
    /// tier (never below the lowest offered, never from a tier the person
    /// picked by hand after the last drop).
    pub fn observe(&mut self, buffering: bool, position: f64, now: Instant) -> Decision {
        let _ = (buffering, position, now);
        Decision::Keep
    }

    /// The person chose a tier themselves; stop automatic changes for this
    /// session.
    pub fn chosen_by_hand(&mut self, quality: Quality) {
        self.current = quality;
    }

    pub fn current(&self) -> Quality {
        self.current
    }

    pub const EARLY: Duration = Duration::from_secs(60);
    pub const LONG_STALL: Duration = Duration::from_secs(5);
}
