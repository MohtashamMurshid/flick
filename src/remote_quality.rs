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
    /// When the current tier started playing; the early window runs from here.
    tier_started: Instant,
    /// Stalls counted toward the two-stall rule on this tier.
    early_stalls: u32,
    /// Whether the last observation was buffering.
    was_buffering: bool,
    /// When the stall in progress began; None when it is not to be timed
    /// (not buffering, or a stall carried over from the tier just left).
    stall_started: Option<Instant>,
    /// The last observed position and time, to recognise seeks.
    last: Option<(f64, Instant)>,
    /// When the last seek was seen.
    last_seek: Option<Instant>,
    /// The person picked a tier; no more automatic changes.
    by_hand: bool,
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
        Self {
            current,
            offered,
            tier_started: started,
            early_stalls: 0,
            was_buffering: false,
            stall_started: None,
            last: None,
            last_seek: None,
            by_hand: false,
        }
    }

    /// Called every tick with libmpv's `paused-for-cache` flag and the
    /// playback position. The rule: two stalls within the first minute of
    /// playback, or one stall longer than five seconds at any time, drop one
    /// tier (never below the lowest offered, never from a tier the person
    /// picked by hand after the last drop).
    ///
    /// A stall is a change into buffering. Each drop restarts the clock for
    /// the new tier, and a stall still under way when the tier changes is
    /// not held against the new one. A short stall that starts within
    /// [`Self::SEEK_GRACE`] of a seek (a jump in `position`) is the seek
    /// refilling the cache, not the link failing, and is not counted; a
    /// long one still is.
    pub fn observe(&mut self, buffering: bool, position: f64, now: Instant) -> Decision {
        if self.seeked(position, now) {
            self.last_seek = Some(now);
        }
        self.last = Some((position, now));

        let began = buffering && !self.was_buffering;
        self.was_buffering = buffering;
        if !buffering {
            self.stall_started = None;
            return Decision::Keep;
        }
        if began {
            self.stall_started = Some(now);
            let after_seek = self
                .last_seek
                .is_some_and(|seek| now.saturating_duration_since(seek) <= Self::SEEK_GRACE);
            let early = now.saturating_duration_since(self.tier_started) <= Self::EARLY;
            if early && !after_seek {
                self.early_stalls += 1;
                if self.early_stalls >= 2 {
                    return self.drop_tier(now, "after buffering twice");
                }
            }
        }
        if let Some(started) = self.stall_started
            && now.saturating_duration_since(started) > Self::LONG_STALL
        {
            return self.drop_tier(now, "after a long pause to buffer");
        }
        Decision::Keep
    }

    /// The person chose a tier themselves; stop automatic changes for this
    /// session.
    pub fn chosen_by_hand(&mut self, quality: Quality) {
        self.current = quality;
        self.by_hand = true;
    }

    pub fn current(&self) -> Quality {
        self.current
    }

    pub const EARLY: Duration = Duration::from_secs(60);
    pub const LONG_STALL: Duration = Duration::from_secs(5);
    /// A stall this soon after a seek is the seek's own refill.
    pub const SEEK_GRACE: Duration = Duration::from_secs(2);

    /// Whether `position` jumped further than playback alone could move it
    /// since the last observation.
    fn seeked(&self, position: f64, now: Instant) -> bool {
        let Some((last_position, last_at)) = self.last else {
            return false;
        };
        let elapsed = now.saturating_duration_since(last_at).as_secs_f64();
        (position - last_position).abs() > elapsed + 1.0
    }

    /// The next tier down from the current one among those offered, or None
    /// at the bottom. A current tier the server does not list counts from
    /// its place in the ladder.
    fn lower(&self) -> Option<Quality> {
        let rank = |quality: Quality| Quality::ALL.iter().position(|q| *q == quality);
        let current = rank(self.current)?;
        self.offered
            .iter()
            .copied()
            .filter_map(|quality| rank(quality).map(|r| (r, quality)))
            .filter(|(r, _)| *r > current)
            .min_by_key(|(r, _)| *r)
            .map(|(_, quality)| quality)
    }

    fn drop_tier(&mut self, now: Instant, why: &str) -> Decision {
        // Whatever happens, this stall has been judged.
        self.stall_started = None;
        if self.by_hand {
            return Decision::Keep;
        }
        let Some(to) = self.lower() else {
            return Decision::Keep;
        };
        self.current = to;
        self.tier_started = now;
        self.early_stalls = 0;
        self.last = None;
        self.last_seek = None;
        Decision::Drop {
            to,
            reason: format!("Dropped to {} {why}.", to.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake clock and a playhead that advances while not buffering.
    struct Session {
        quality: AutoQuality,
        start: Instant,
        at: Duration,
        position: f64,
    }

    const TICK: Duration = Duration::from_millis(100);

    impl Session {
        fn new(current: Quality, offered: &[Quality]) -> Self {
            let start = Instant::now();
            Self {
                quality: AutoQuality::new(current, offered.to_vec(), start),
                start,
                at: Duration::ZERO,
                position: 0.0,
            }
        }

        fn tick(&mut self, buffering: bool) -> Decision {
            self.at += TICK;
            if !buffering {
                self.position += TICK.as_secs_f64();
            }
            self.quality
                .observe(buffering, self.position, self.start + self.at)
        }

        /// Plays or buffers for `length`, returning every non-Keep decision.
        fn run(&mut self, buffering: bool, length: Duration) -> Vec<Decision> {
            let ticks = (length.as_millis() / TICK.as_millis()) as usize;
            (0..ticks)
                .map(|_| self.tick(buffering))
                .filter(|d| *d != Decision::Keep)
                .collect()
        }

        fn play(&mut self, seconds: u64) -> Vec<Decision> {
            self.run(false, Duration::from_secs(seconds))
        }

        fn buffer(&mut self, length: Duration) -> Vec<Decision> {
            self.run(true, length)
        }

        fn seek(&mut self, to: f64) {
            self.position = to;
        }
    }

    const ALL: [Quality; 5] = Quality::ALL;
    const SHORT: Duration = Duration::from_secs(1);

    fn drop_to(to: Quality, reason: &str) -> Decision {
        Decision::Drop {
            to,
            reason: reason.into(),
        }
    }

    #[test]
    fn steady_playback_keeps_the_tier() {
        let mut s = Session::new(Quality::Original, &ALL);
        assert!(s.play(600).is_empty());
        assert_eq!(s.quality.current(), Quality::Original);
    }

    #[test]
    fn two_early_stalls_drop_one_tier() {
        let mut s = Session::new(Quality::Original, &ALL);
        assert!(s.play(10).is_empty());
        assert!(s.buffer(SHORT).is_empty());
        assert!(s.play(10).is_empty());
        assert_eq!(
            s.buffer(SHORT),
            vec![drop_to(
                Quality::High1080,
                "Dropped to 1080p High after buffering twice."
            )]
        );
        assert_eq!(s.quality.current(), Quality::High1080);
    }

    #[test]
    fn stalls_after_the_first_minute_do_not_count_toward_two() {
        let mut s = Session::new(Quality::Standard1080, &ALL);
        assert!(s.play(30).is_empty());
        assert!(s.buffer(SHORT).is_empty());
        assert!(s.play(40).is_empty());
        // 71 s in: past the early window.
        assert!(s.buffer(SHORT).is_empty());
        assert!(s.play(30).is_empty());
        assert!(s.buffer(SHORT).is_empty());
        assert_eq!(s.quality.current(), Quality::Standard1080);
    }

    #[test]
    fn one_long_stall_drops_at_any_time() {
        let mut s = Session::new(Quality::Standard1080, &ALL);
        assert!(s.play(300).is_empty());
        // Five seconds exactly is still patient.
        assert!(s.buffer(Duration::from_secs(5)).is_empty());
        let decisions = s.buffer(Duration::from_secs(10));
        assert_eq!(
            decisions,
            vec![drop_to(
                Quality::Low720,
                "Dropped to 720p after a long pause to buffer."
            )],
            "one drop per stall, however long it lasts"
        );
    }

    #[test]
    fn each_drop_restarts_the_clock_for_the_new_tier() {
        let mut s = Session::new(Quality::Original, &ALL);
        s.play(5);
        s.buffer(SHORT);
        s.play(5);
        assert_eq!(s.buffer(SHORT).len(), 1);
        assert_eq!(s.quality.current(), Quality::High1080);
        // The stall that caused the drop is not held against the new tier,
        // even when the reload keeps buffering for a while.
        assert!(s.buffer(Duration::from_secs(8)).is_empty());
        // A single stall on the new tier is not enough...
        s.play(35);
        assert!(s.buffer(SHORT).is_empty());
        // ...and 66 s into the old clock, the new tier's minute still runs.
        s.play(10);
        assert_eq!(
            s.buffer(SHORT),
            vec![drop_to(
                Quality::Standard1080,
                "Dropped to 1080p after buffering twice."
            )]
        );
    }

    #[test]
    fn never_below_the_lowest_offered() {
        let offered = [Quality::Original, Quality::Low720];
        let mut s = Session::new(Quality::Original, &offered);
        s.play(2);
        s.buffer(SHORT);
        s.play(2);
        // Tiers the server does not offer are skipped.
        assert_eq!(
            s.buffer(SHORT),
            vec![drop_to(
                Quality::Low720,
                "Dropped to 720p after buffering twice."
            )]
        );
        s.play(2);
        s.buffer(SHORT);
        s.play(2);
        assert!(s.buffer(SHORT).is_empty());
        assert!(s.buffer(Duration::from_secs(20)).is_empty());
        assert_eq!(s.quality.current(), Quality::Low720);
    }

    #[test]
    fn original_only_never_drops() {
        let mut s = Session::new(Quality::Original, &[Quality::Original]);
        s.play(2);
        s.buffer(SHORT);
        s.play(2);
        assert!(s.buffer(Duration::from_secs(30)).is_empty());
        let mut s = Session::new(Quality::Original, &[]);
        assert!(s.buffer(Duration::from_secs(30)).is_empty());
    }

    #[test]
    fn a_tier_not_offered_drops_to_the_nearest_lower() {
        let offered = [Quality::Original, Quality::Standard1080, Quality::Data480];
        let mut s = Session::new(Quality::High1080, &offered);
        s.play(2);
        assert_eq!(
            s.buffer(Duration::from_secs(6)),
            vec![drop_to(
                Quality::Standard1080,
                "Dropped to 1080p after a long pause to buffer."
            )]
        );
        let mut s = Session::new(Quality::Low720, &offered);
        s.play(2);
        assert_eq!(
            s.buffer(Duration::from_secs(6))[0],
            drop_to(
                Quality::Data480,
                "Dropped to 480p after a long pause to buffer."
            )
        );
    }

    #[test]
    fn a_tier_chosen_by_hand_is_never_changed() {
        let mut s = Session::new(Quality::Original, &ALL);
        s.play(2);
        s.quality.chosen_by_hand(Quality::Standard1080);
        assert_eq!(s.quality.current(), Quality::Standard1080);
        s.buffer(SHORT);
        s.play(2);
        assert!(s.buffer(SHORT).is_empty());
        assert!(s.buffer(Duration::from_secs(30)).is_empty());
        assert_eq!(s.quality.current(), Quality::Standard1080);
    }

    #[test]
    fn choosing_by_hand_after_a_drop_stops_further_drops() {
        let mut s = Session::new(Quality::Original, &ALL);
        s.play(2);
        assert_eq!(s.buffer(Duration::from_secs(6)).len(), 1);
        s.quality.chosen_by_hand(Quality::Original);
        s.play(2);
        assert!(s.buffer(Duration::from_secs(6)).is_empty());
        assert_eq!(s.quality.current(), Quality::Original);
    }

    #[test]
    fn a_stall_right_after_a_seek_is_not_counted() {
        let mut s = Session::new(Quality::Original, &ALL);
        s.play(5);
        s.seek(600.0);
        assert!(s.buffer(SHORT).is_empty());
        s.play(5);
        s.seek(30.0);
        assert!(s.buffer(SHORT).is_empty());
        s.play(5);
        // A stall with no seek before it still counts, once.
        assert!(s.buffer(SHORT).is_empty());
        s.play(5);
        assert_eq!(s.buffer(SHORT).len(), 1);
    }

    #[test]
    fn a_long_stall_after_a_seek_still_counts() {
        let mut s = Session::new(Quality::Original, &ALL);
        s.play(5);
        s.seek(600.0);
        assert_eq!(s.buffer(Duration::from_secs(6)).len(), 1);
    }

    #[test]
    fn a_stall_from_the_first_tick_counts() {
        let mut s = Session::new(Quality::Original, &ALL);
        assert!(s.buffer(SHORT).is_empty());
        s.play(1);
        assert_eq!(s.buffer(SHORT).len(), 1);
    }
}
