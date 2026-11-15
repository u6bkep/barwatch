//! The time axis: how far it reaches, where a moment lands on it, and how
//! numbers and times are written. Pure functions, so the rules are testable.

use std::time::Instant;

/// Lengths the axis may take while the future is hidden, in minutes. The axis
/// steps along these as the game goes on, so its length says nothing about how
/// long the game will last.
const HORIZON_LADDER_MIN: [f32; 14] = [10.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0, 75.0, 90.0, 120.0, 150.0, 180.0, 240.0];

/// How far the axis reaches, in game seconds.
///
/// Shown, it is the whole game. Hidden, it is the time played so far plus room
/// to look ahead (at least five minutes, or a third of the time played),
/// rounded up to a round length: the real end of the game never shows through.
/// Once the game is over there is nothing left to hide.
pub fn horizon(game_s: f32, duration_s: f32, hide_future: bool) -> f32 {
    if !hide_future || game_s >= duration_s {
        return duration_s.max(1.0);
    }
    let need = game_s + (game_s / 3.0).max(300.0);
    let minutes = HORIZON_LADDER_MIN.iter().copied().find(|m| m * 60.0 >= need);
    minutes.map(|m| m * 60.0).unwrap_or_else(|| (need / 3600.0).ceil() * 3600.0)
}

/// Where `t` falls on an axis that ends at `end_s`, from 0 to 1.
pub fn frac(t: f32, end_s: f32) -> f32 {
    (t / end_s.max(1e-3)).clamp(0.0, 1.0)
}

/// The moment under a pointer at `x`, on an axis drawn from `left` and `width` px wide.
pub fn time_at(x: f32, left: f32, width: f32, end_s: f32) -> f32 {
    ((x - left) / width.max(1.0)).clamp(0.0, 1.0) * end_s
}

/// A gap between ticks that gives about `max_ticks` of them.
pub fn tick_step(end_s: f32, max_ticks: f32) -> f32 {
    [60.0, 120.0, 300.0, 600.0, 900.0, 1200.0, 1800.0, 3600.0].into_iter().find(|s| end_s / s <= max_ticks).unwrap_or(7200.0)
}

/// The box a tick's label is drawn in, px: the text is about 40 wide, centred.
pub const TICK_LABEL_BOX: f32 = 56.0;

/// A tick of the ruler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    /// Its moment, in game seconds.
    pub t: f32,
    /// Whether its label is written: not where something else sits on the ruler.
    pub labelled: bool,
}

/// The ticks of a ruler `w` px wide that ends at `end_s`. A label is centred
/// on its tick but kept inside the axis, so the first and the last sit
/// off-centre; it is left out where it would run into one of `taken` (spans
/// in px along the axis: the position chips).
pub fn ticks(end_s: f32, w: f32, taken: &[(f32, f32)]) -> Vec<Tick> {
    let step = tick_step(end_s, (w / 90.0).max(2.0));
    let half = TICK_LABEL_BOX / 2.0;
    // The text and a little air either side.
    let reach = 24.0;
    let mut out = Vec::new();
    let mut t = 0.0;
    while t <= end_s + 0.5 {
        let x = (frac(t, end_s) * w).clamp(half, (w - half).max(half));
        let labelled = !taken.iter().any(|(from, to)| x + reach > *from && x - reach < *to);
        out.push(Tick { t, labelled });
        t += step;
    }
    out
}

pub fn clock(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u32;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A number in at most four characters or so, the way BAR's panel writes them.
pub fn number(v: f32) -> String {
    let a = v.abs();
    if a >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if a >= 10_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if a >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if a >= 100.0 || (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// The smallest "round" number at or above `v`, for a chart's top edge.
pub fn nice_ceil(v: f32) -> f32 {
    if v <= 0.0 {
        return 1.0;
    }
    let mag = 10f32.powf(v.log10().floor());
    let lead = v / mag;
    let step = [1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0].into_iter().find(|s| lead <= *s + 1e-4).unwrap_or(10.0);
    step * mag
}

/// A value that follows its target smoothly: chart scales and the axis length
/// glide to a new value instead of jumping.
#[derive(Clone, Copy, Debug)]
pub struct Eased {
    pub value: f32,
    target: f32,
    last: Option<Instant>,
}

/// Time constant of the glide, in seconds.
const EASE_TAU_S: f32 = 0.16;

impl Eased {
    pub fn new(value: f32) -> Eased {
        Eased { value, target: value, last: None }
    }

    /// Moves toward `target` by the time passed since the last call; returns
    /// whether it is still moving.
    pub fn follow(&mut self, target: f32, now: Instant) -> bool {
        self.target = target;
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f32()).unwrap_or(0.0).min(0.1);
        self.last = Some(now);
        let k = 1.0 - (-dt / EASE_TAU_S).exp();
        self.value += (target - self.value) * k;
        let settled = (self.value - target).abs() <= target.abs() * 0.002 + 1e-3;
        if settled {
            self.value = target;
        }
        !settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_horizon_does_not_depend_on_the_game_length() {
        for game in [0.0, 200.0, 600.0, 1500.0, 2300.0] {
            let short = horizon(game, 2400.0, true);
            let long = horizon(game, 9000.0, true);
            assert_eq!(short, long, "at {game}s");
            assert!(short >= game + 300.0, "room to look ahead at {game}s");
        }
    }

    #[test]
    fn hidden_horizon_steps_through_round_lengths() {
        assert_eq!(horizon(0.0, 2400.0, true), 600.0);
        assert_eq!(horizon(400.0, 2400.0, true), 900.0);
        assert_eq!(horizon(1500.0, 2400.0, true), 2400.0);
        assert_eq!(horizon(14000.0, 30000.0, true), 21600.0);
    }

    #[test]
    fn shown_or_finished_games_span_their_length() {
        assert_eq!(horizon(100.0, 2400.0, false), 2400.0);
        assert_eq!(horizon(2400.0, 2400.0, true), 2400.0);
    }

    #[test]
    fn pixels_and_time_round_trip() {
        let end = 1800.0;
        for t in [0.0, 1.0, 900.0, 1799.0] {
            let x = 40.0 + frac(t, end) * 600.0;
            assert!((time_at(x, 40.0, 600.0, end) - t).abs() < 0.01);
        }
        assert_eq!(time_at(-50.0, 40.0, 600.0, end), 0.0);
        assert_eq!(time_at(5000.0, 40.0, 600.0, end), end);
    }

    #[test]
    fn tick_labels_give_way_to_what_sits_on_the_ruler() {
        // 40 minutes on 900 px: a tick every five minutes, nine in all.
        let all = ticks(2400.0, 900.0, &[]);
        assert_eq!(all.len(), 9);
        assert!(all.iter().all(|t| t.labelled));
        // A chip over 15:00 hides that label and no other.
        let x = frac(900.0, 2400.0) * 900.0;
        let hidden: Vec<f32> =
            ticks(2400.0, 900.0, &[(x - 25.0, x + 25.0)]).iter().filter(|t| !t.labelled).map(|t| t.t).collect();
        assert_eq!(hidden, vec![900.0]);
        // The last label is drawn inside the axis, left of its tick: a chip
        // that ends 20 px short of the edge still runs into it.
        let last = *ticks(2400.0, 900.0, &[(700.0, 880.0)]).last().unwrap();
        assert_eq!(last, Tick { t: 2400.0, labelled: false });
        // ... and one that ends well before it does not.
        assert!(ticks(2400.0, 900.0, &[(700.0, 840.0)]).last().unwrap().labelled);
    }

    #[test]
    fn numbers_and_clocks() {
        assert_eq!(clock(65.4), "1:05");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(number(12.34), "12.3");
        assert_eq!(number(12.0), "12");
        assert_eq!(number(1234.0), "1.2k");
        assert_eq!(number(25_000.0), "25k");
        assert_eq!(nice_ceil(230.0), 300.0);
        assert_eq!(nice_ceil(300.0), 300.0);
        assert_eq!(nice_ceil(0.0), 1.0);
    }

    #[test]
    fn eased_values_settle_on_their_target() {
        let start = Instant::now();
        let mut e = Eased::new(0.0);
        e.follow(100.0, start);
        let mut moving = true;
        for i in 1..200 {
            moving = e.follow(100.0, start + std::time::Duration::from_millis(16 * i));
        }
        assert!(!moving);
        assert_eq!(e.value, 100.0);
    }
}
