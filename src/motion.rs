use crate::{config::Easing, lyrics::Line};
use std::time::{Duration, Instant};

pub fn next_frame(deadline: Instant, finished: Instant, period: Duration) -> Instant {
    if finished < deadline {
        return deadline;
    }
    let elapsed = finished.saturating_duration_since(deadline).as_nanos();
    let remainder = elapsed % period.as_nanos();
    finished + period - Duration::from_nanos(remainder as u64)
}

pub fn ease(curve: Easing, t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    match curve {
        Easing::InOut => {
            if t < 0.5 {
                4.0 * t.powi(3)
            } else {
                1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
            }
        }
        Easing::Out => 1.0 - (1.0 - t).powi(3),
        Easing::Spring => {
            (1.0 - (1.0 + 7.0 * t) * (-7.0 * t).exp()) / (1.0 - 8.0 * (-7.0_f64).exp())
        }
    }
}

/// Moves through the document at a steady cadence between whole terminal rows.
pub struct Scroll {
    pub current_scroll_offset: f64,
    pub target_scroll_offset: f64,
    rows_per_second: f64,
    updated: Instant,
    presented_row: isize,
}

impl Scroll {
    pub fn new(offset: f64, now: Instant) -> Self {
        Self {
            current_scroll_offset: offset,
            target_scroll_offset: offset,
            rows_per_second: 0.0,
            updated: now,
            presented_row: offset.round() as isize,
        }
    }

    pub fn advance(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        let remaining = self.target_scroll_offset - self.current_scroll_offset;
        let step = self.rows_per_second * dt;
        if remaining.abs() <= step {
            self.current_scroll_offset = self.target_scroll_offset;
        } else {
            self.current_scroll_offset += step.copysign(remaining);
        }
    }

    pub fn retarget(&mut self, target: f64, duration_ms: u64) {
        let distance = (target - self.current_scroll_offset).abs();
        // Treat the configured duration as the time to travel at least eight rows.
        // A short transition then finishes promptly at the same steady cadence.
        self.rows_per_second =
            distance.max(8.0) / Duration::from_millis(duration_ms.max(1)).as_secs_f64();
        self.target_scroll_offset = target;
    }

    pub fn present(&mut self) {
        // Quantize only here. If frames were delayed or a wrapped block is tall,
        // let the displayed document catch up one whole terminal row at a time.
        let wanted = self.current_scroll_offset.round() as isize;
        self.presented_row += (wanted - self.presented_row).clamp(-1, 1);
    }

    pub fn row(&self) -> isize {
        self.presented_row
    }

    pub fn running(&self) -> bool {
        self.current_scroll_offset != self.target_scroll_offset
            || self.presented_row != self.target_scroll_offset.round() as isize
    }
}

pub struct Tween {
    from: f64,
    to: f64,
    start: Instant,
    duration: Duration,
    curve: Easing,
}

impl Tween {
    pub fn fixed(value: f64, now: Instant) -> Self {
        Self {
            from: value,
            to: value,
            start: now,
            duration: Duration::ZERO,
            curve: Easing::Out,
        }
    }

    pub fn value(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return self.to;
        }
        let time =
            now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64();
        self.from + (self.to - self.from) * ease(self.curve, time)
    }

    pub fn running(&self, now: Instant) -> bool {
        (self.from - self.to).abs() > f64::EPSILON
            && now.saturating_duration_since(self.start) < self.duration
    }

    pub fn retarget(&mut self, to: f64, now: Instant, ms: u64, curve: Easing) {
        *self = Self {
            from: self.value(now),
            to,
            start: now,
            duration: Duration::from_millis(ms),
            curve,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub fn mix(self, other: Self, weight: f64) -> Self {
        let mix = |a: u8, b: u8| {
            (f64::from(a) + (f64::from(b) - f64::from(a)) * weight.clamp(0.0, 1.0)).round() as u8
        };
        Self(
            mix(self.0, other.0),
            mix(self.1, other.1),
            mix(self.2, other.2),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WordState {
    Upcoming,
    Active(f64),
    Complete,
}

pub fn word_state(
    line: &Line,
    index: usize,
    position: i64,
    smooth: bool,
    pulse: bool,
) -> WordState {
    let word = &line.words[index];
    let end = word
        .end
        .or_else(|| line.words.get(index + 1).map(|w| w.start))
        .or(line.end);
    if position < word.start {
        return WordState::Upcoming;
    }
    if end.is_some_and(|end| position >= end) {
        return WordState::Complete;
    }
    if !smooth {
        return WordState::Active(1.0);
    }
    let age = (position - word.start) as f64;
    let attack = ease(Easing::Out, age / 90.0);
    let release = end.map_or(1.0, |end| {
        ease(Easing::Out, (end - position) as f64 / 100.0)
    });
    let shimmer = if pulse {
        0.94 + 0.06 * (age.min(1200.0) * std::f64::consts::TAU / 1200.0).cos()
    } else {
        1.0
    };
    WordState::Active(attack * release * shimmer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::lrc;

    #[test]
    fn short_scroll_presents_rows_at_regular_intervals() {
        let now = Instant::now();
        let mut scroll = Scroll::new(0.0, now);
        scroll.retarget(3.0, 640);

        for (ms, expected) in [(39, 0), (41, 1), (119, 1), (121, 2), (199, 2), (201, 3)] {
            scroll.advance(now + Duration::from_millis(ms));
            scroll.present();
            assert_eq!(scroll.row(), expected, "at {ms} ms");
        }
        scroll.advance(now + Duration::from_millis(241));
        assert!(!scroll.running());
    }

    #[test]
    fn scroll_retargets_from_its_current_position() {
        let now = Instant::now();
        let mut scroll = Scroll::new(0.0, now);
        scroll.retarget(3.0, 640);
        let midpoint = now + Duration::from_millis(80);
        scroll.advance(midpoint);
        scroll.present();
        assert_eq!(scroll.row(), 1);
        scroll.retarget(-2.0, 640);
        assert_eq!(scroll.current_scroll_offset, 1.0);
        scroll.advance(midpoint + Duration::from_millis(80));
        scroll.present();
        assert_eq!(scroll.row(), 0);
    }

    #[test]
    fn late_frame_never_skips_presented_terminal_rows() {
        let now = Instant::now();
        let mut scroll = Scroll::new(0.0, now);
        scroll.retarget(12.5, 650);
        scroll.advance(now + Duration::from_secs(2));
        for row in 1..=13 {
            scroll.present();
            assert_eq!(scroll.row(), row);
        }
        assert!(!scroll.running());
    }

    #[test]
    fn frame_clock_preserves_phase_and_skips_missed_frames() {
        let first = Instant::now();
        let frame = Duration::from_secs_f64(1.0 / 60.0);
        assert_eq!(
            next_frame(first, first + Duration::from_millis(1), frame),
            first + frame
        );
        assert_eq!(
            next_frame(first, first + frame * 4, frame),
            first + frame * 5
        );
        assert_eq!(next_frame(first + frame, first, frame), first + frame);
    }

    #[test]
    fn retargeting_keeps_current_document_position() {
        let now = Instant::now();
        let mut tween = Tween::fixed(0.0, now);
        tween.retarget(10.0, now, 400, Easing::InOut);
        let midpoint = now + Duration::from_millis(200);
        assert_eq!(tween.value(midpoint), 5.0);
        tween.retarget(-5.0, midpoint, 160, Easing::Out);
        assert_eq!(tween.value(midpoint), 5.0);
        assert_eq!(tween.value(midpoint + Duration::from_millis(160)), -5.0);
    }

    #[test]
    fn distinguishes_word_boundaries_and_highlight_strength() {
        let line = &lrc("[00:01]<00:01>Світ <00:02>тихий<00:03>").lines[0];
        assert_eq!(word_state(line, 1, 1999, true, false), WordState::Upcoming);
        assert_eq!(word_state(line, 0, 2000, true, false), WordState::Complete);
        assert_eq!(
            word_state(line, 1, 2200, true, false),
            WordState::Active(1.0)
        );
        assert_eq!(word_state(line, 1, 1650, true, false), WordState::Upcoming);
    }
}
