//! The most memory a running job can still reach, learned from the curves of
//! its past runs (`reserve = "remaining"`).
//!
//! A job is admitted against the peak of its history, and a running job
//! reserves what it can still grow into: `need - live`. With a flat need, a
//! job at 2 GB that peaked at 20 GB early in its run keeps 18 GB reserved
//! until it ends, long after its peak is over. Each run records the highest
//! memory it reached from each moment of its run on (`Curve`), and a running
//! job's need becomes the highest any recent run still reached from its age
//! on (`Envelope::at`), plus a margin. The rules come from the research on
//! issue #7:
//!
//! - the maximum over the runs, never a percentile;
//! - widened in time by a fifth of the age, at least 10 s, as runs vary in
//!   length;
//! - 15% on top;
//! - a run that goes above it books its whole need again, at once, for the
//!   rest of the run (see `Entry::reach`);
//! - past the end of the longest run, the whole need again;
//! - with fewer than three recorded curves, the whole need;
//! - never more than the whole need: outliers, failed runs and minimums
//!   rule that, as before.

use serde::{Deserialize, Serialize};

/// Runs with a recorded curve before the curve counts.
pub const MIN_RUNS: usize = 3;
/// Room above the curve.
pub const MARGIN: f64 = 1.15;
/// The curve is widened in time by this share of the age...
pub const WIDEN: f64 = 0.2;
/// ...and at least this many seconds.
pub const WIDEN_MIN_S: f64 = 10.0;

/// One run's memory, as the highest it reached from each moment on: from
/// `p[i].0` seconds of its run until the next point, it still reached
/// `p[i].1` kB at some later moment. The values only fall. `d` is how long
/// the run ran (paused time left out). Stored as JSON in `runs.reach`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    pub d: f64,
    pub p: Vec<(f64, u64)>,
}

impl Curve {
    /// From a run's readings, `(age in seconds, kB)` in time order, and how
    /// long it ran. A drop smaller than a fiftieth of the peak, or 32 MB, is
    /// not kept: the curve stays at the higher value until it falls by more,
    /// so it never reads lower than the run was.
    pub fn from_samples(samples: &[(f64, u64)], d: f64) -> Option<Curve> {
        let peak = samples.iter().map(|s| s.1).max()?;
        let mut suffix = vec![0u64; samples.len()];
        let mut best = 0;
        for i in (0..samples.len()).rev() {
            best = best.max(samples[i].1);
            suffix[i] = best;
        }
        let tol = (peak / 50).max(32 * 1024);
        let mut p = vec![(0.0, peak)];
        for (i, &(age, _)) in samples.iter().enumerate() {
            let cur = p.last().map_or(peak, |l| l.1);
            // From this reading on, the run no longer reached the higher value.
            if suffix[i] + tol < cur {
                p.push((age, suffix[i]));
            }
        }
        Some(Curve { d, p })
    }

    /// A run with no recorded curve: its peak for its whole length.
    pub fn flat(peak_kb: u64, d: f64) -> Curve {
        Curve { d, p: vec![(0.0, peak_kb)] }
    }

    /// The highest it reached from `age` on; 0 past its end.
    fn at(&self, age: f64) -> u64 {
        if age > self.d {
            return 0;
        }
        self.p.iter().take_while(|(a, _)| *a <= age).last().map_or(0, |(_, kb)| *kb)
    }
}

/// The curves of a job's recent runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Envelope {
    pub curves: Vec<Curve>,
}

impl Envelope {
    /// The recent runs' curves, `recorded` of them recorded and the rest
    /// flat. None with fewer than `MIN_RUNS` recorded: then the whole need
    /// counts.
    pub fn of(curves: Vec<Curve>, recorded: usize) -> Option<Envelope> {
        (recorded >= MIN_RUNS).then_some(Envelope { curves })
    }

    /// The most a run at `age` seconds can still reach, by its past runs:
    /// the highest any of them reached from a little before that age on.
    /// None past the end of the longest of them.
    pub fn at(&self, age: f64) -> Option<u64> {
        let from = (age - (age * WIDEN).max(WIDEN_MIN_S)).max(0.0);
        let longest = self.curves.iter().map(|c| c.d).fold(0.0, f64::max);
        (from <= longest).then(|| self.curves.iter().map(|c| c.at(from)).max().unwrap_or(0))
    }

    /// What a run at `age` books by it: that, plus the margin.
    pub fn need_at(&self, age: f64) -> Option<u64> {
        self.at(age).map(|kb| (kb as f64 * MARGIN) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;

    /// A `ci` run: 1 GB, a 6 GB peak around 60 s, then 1 GB again until 120 s.
    fn ci_run(shift: f64) -> Vec<(f64, u64)> {
        (0..=60).map(|i| i as f64 * 2.0 + shift).map(|t| (t, if (50.0..=70.0).contains(&(t - shift)) { 6 * GB } else { GB })).collect()
    }

    #[test]
    fn a_curve_holds_the_highest_still_to_come() {
        let c = Curve::from_samples(&ci_run(0.0), 120.0).unwrap();
        assert_eq!(c.p, vec![(0.0, 6 * GB), (72.0, GB)]);
        assert_eq!((c.at(0.0), c.at(71.0), c.at(72.0), c.at(121.0)), (6 * GB, 6 * GB, GB, 0));
        // A small dip is not kept: the curve never reads lower than the run.
        let wobble = [(0.0, 1000 * 1024), (2.0, 990 * 1024), (4.0, 980 * 1024)];
        assert_eq!(Curve::from_samples(&wobble, 4.0).unwrap().p, vec![(0.0, 1000 * 1024)]);
        assert_eq!(Curve::from_samples(&[], 1.0), None);
    }

    #[test]
    fn a_running_job_books_only_what_its_past_runs_still_reached() {
        let curves: Vec<Curve> = [0.0, 4.0, -4.0].iter().map(|s| Curve::from_samples(&ci_run(*s), 120.0).unwrap()).collect();
        let e = Envelope::of(curves, 3).unwrap();
        // Before and around the peak, the whole peak.
        assert_eq!(e.at(10.0), Some(6 * GB));
        assert_eq!(e.at(80.0), Some(6 * GB), "widened: 80 s looks from 64 s on");
        // Well past it, what is left.
        assert_eq!(e.at(100.0), Some(GB));
        assert_eq!(e.need_at(100.0), Some((GB as f64 * MARGIN) as u64));
        // Past the end of the longest run (widened), the whole need again.
        assert_eq!(e.at(135.0), Some(GB));
        assert_eq!(e.at(160.0), None);
    }

    #[test]
    fn a_run_without_a_curve_counts_at_its_peak_and_too_few_curves_count_for_nothing() {
        let recorded: Vec<Curve> = (0..3).map(|_| Curve::from_samples(&ci_run(0.0), 120.0).unwrap()).collect();
        let mut curves = recorded.clone();
        curves.push(Curve::flat(5 * GB, 100.0));
        let e = Envelope::of(curves, 3).unwrap();
        assert_eq!(e.at(100.0), Some(5 * GB), "an older run's shape is unknown: its peak counts until it ended");
        assert_eq!(e.at(130.0), Some(GB), "past its end, widened: what the recorded runs still reached");
        assert_eq!(Envelope::of(recorded[..2].to_vec(), 2), None);
    }

    #[test]
    fn a_short_run_does_not_shrink_what_a_long_run_still_reaches() {
        // A cache hit that ends in 5 s next to two cold runs that peak late.
        let late = |t: f64| (0..=150).map(|i| i as f64 * 2.0 + t).map(|a| (a, if a > 250.0 { 8 * GB } else { GB })).collect::<Vec<_>>();
        let curves = vec![
            Curve::from_samples(&[(1.0, GB / 4), (3.0, GB / 4)], 5.0).unwrap(),
            Curve::from_samples(&late(0.0), 300.0).unwrap(),
            Curve::from_samples(&late(0.0), 300.0).unwrap(),
        ];
        let e = Envelope::of(curves, 3).unwrap();
        assert_eq!(e.at(100.0), Some(8 * GB));
    }
}
