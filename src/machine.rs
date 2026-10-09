//! One shared reading of the machine, cached in a file so that the many
//! waiting jobs never each pay for a measurement. The recorder refreshes it
//! every couple of seconds; when no recorder runs, the first waiter that finds
//! it stale refreshes it under the queue lock.

use crate::sys;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The shared `machine` cache file. Every taskguard version on the machine
/// reads it, so the rules on `queue::Entry` hold here too: never remove or
/// rename a field, never change its type, and new fields are optional. A
/// version that cannot read the file takes its own reading instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MachineSample {
    pub ts: f64,
    /// Busy cores, smoothed over about three seconds. The scheduler uses this.
    pub cpu_busy: f64,
    /// Busy cores over the last interval alone. The history records this.
    pub cpu_inst: f64,
    pub ncpu: usize,
    pub mem_used_kb: u64,
    pub mem_total_kb: u64,
    pub mem_pressure: Option<f64>,
    /// Readings of the last `PEAK_WINDOW` seconds, oldest first: the time,
    /// the memory in use, and how much of it taskguard's own jobs held.
    pub recent_mem: Vec<(f64, u64, u64)>,
    /// Readings of the last `LOW_WINDOW` seconds, oldest first: the time and
    /// the busy cores (smoothed). Older versions leave it out.
    pub recent_cpu: Vec<(f64, f64)>,
    pub(crate) ticks_busy: u64,
    pub(crate) ticks_total: u64,
}

impl MachineSample {
    /// A sample with fixed numbers, for tests and snapshots.
    #[cfg(test)]
    pub fn fixed(cpu_busy: f64, ncpu: usize, mem_used_kb: u64, mem_total_kb: u64) -> Self {
        MachineSample { ts: 0.0, cpu_busy, ncpu, mem_used_kb, mem_total_kb, ..Default::default() }
    }

    /// Memory in use for an admission check: other programs at their highest
    /// of the last few seconds, plus taskguard's own jobs as they are now.
    ///
    /// The latest reading alone is not enough: memory swings while pages move
    /// between lists and a browser opens and closes tabs, and a job started in
    /// a dip lands in the peak that follows. The highest total is not enough
    /// either: when the other programs peaked while our jobs were still small,
    /// and our jobs grew while the others dipped, no single reading shows both.
    pub fn mem_for_admission(&self, ours_now_kb: u64) -> u64 {
        let others_peak = self.recent_mem.iter().map(|(_, used, ours)| used.saturating_sub(*ours)).max().unwrap_or(0);
        (others_peak + ours_now_kb).max(self.mem_used_kb)
    }

    /// The fewest busy cores of the last `LOW_WINDOW` seconds, this reading
    /// included. A reading written by an older version has no history: then
    /// the low is the reading itself.
    pub fn cpu_low(&self) -> f64 {
        self.recent_cpu.iter().filter(|(ts, _)| self.ts - ts < LOW_WINDOW).map(|(_, b)| *b).fold(self.cpu_busy, f64::min)
    }

    /// Is the machine about as quiet as it has been lately? Within one core,
    /// or a tenth of the cores on a big machine, of its recent low.
    pub fn near_cpu_low(&self) -> bool {
        self.cpu_busy <= self.cpu_low() + (0.1 * self.ncpu as f64).max(1.0)
    }

    pub fn mem_pct(&self) -> f64 {
        if self.mem_total_kb == 0 { 0.0 } else { self.mem_used_kb as f64 * 100.0 / self.mem_total_kb as f64 }
    }
}

fn path(dir: &Path) -> std::path::PathBuf {
    dir.join("machine")
}

pub fn read_cache(dir: &Path) -> Option<MachineSample> {
    let text = std::fs::read_to_string(path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_cache(dir: &Path, s: &MachineSample) {
    let tmp = dir.join(format!("machine.tmp.{}", std::process::id()));
    if let Ok(text) = serde_json::to_string(s)
        && std::fs::write(&tmp, text).is_ok()
    {
        let _ = std::fs::rename(&tmp, path(dir));
    }
}

/// Take a new reading, using `prev` for the CPU tick delta. Without a usable
/// previous reading it measures over a quarter second, because CPU load only
/// exists as a difference between two tick counts.
/// `ours_kb` is what taskguard's running jobs hold at this moment.
pub fn measure(prev: Option<&MachineSample>, ours_kb: u64) -> MachineSample {
    let now = crate::db::now();
    let ncpu = sys::ncpu();
    let usable = prev.filter(|p| p.ticks_total > 0 && now - p.ts >= 0.2 && now - p.ts < 30.0);
    let (base_busy, base_total, base_ts, prev_busy) = match usable {
        Some(p) => (p.ticks_busy, p.ticks_total, p.ts, Some(p.cpu_busy)),
        None => {
            let t = sys::cpu_ticks();
            std::thread::sleep(std::time::Duration::from_millis(250));
            (t.busy, t.total, now, None)
        }
    };
    let t = sys::cpu_ticks();
    let now = crate::db::now();
    let used = sys::mem_used_kb();
    let mut recent_mem: Vec<(f64, u64, u64)> = prev.map(|p| p.recent_mem.clone()).unwrap_or_default();
    recent_mem.retain(|(ts, _, _)| now - ts < PEAK_WINDOW);
    recent_mem.push((now, used, ours_kb.min(used)));
    let dt_ticks = t.total.saturating_sub(base_total);
    let inst =
        if dt_ticks > 0 { t.busy.saturating_sub(base_busy) as f64 / dt_ticks as f64 * ncpu as f64 } else { prev_busy.unwrap_or(0.0) };
    // Exponential smoothing with a time constant of about three seconds, so one
    // short spike does not block or release a queue of jobs by itself.
    let cpu_busy = match prev_busy {
        Some(p) => {
            let alpha = ((now - base_ts) / 3.0).clamp(0.0, 1.0);
            alpha * inst + (1.0 - alpha) * p
        }
        None => inst,
    };
    let mut recent_cpu: Vec<(f64, f64)> =
        prev.map(|p| p.recent_cpu.iter().filter(|(ts, _)| now - ts < LOW_WINDOW).copied().collect()).unwrap_or_default();
    recent_cpu.push((now, cpu_busy));
    MachineSample {
        ts: now,
        cpu_busy,
        cpu_inst: inst,
        ncpu,
        mem_used_kb: used,
        mem_total_kb: sys::mem_total_kb(),
        // TASKGUARD_PRESSURE fixes the reading, for tests that must not
        // depend on how busy the machine that runs them is.
        mem_pressure: match std::env::var("TASKGUARD_PRESSURE") {
            Ok(v) => v.parse().ok(),
            Err(_) => sys::mem_pressure_pct(),
        },
        recent_mem,
        recent_cpu,
        ticks_busy: t.busy,
        ticks_total: t.total,
    }
}

/// Seconds of memory readings the admission check takes the highest of.
pub const PEAK_WINDOW: f64 = 10.0;

/// Seconds of CPU readings a job that cannot fully fit looks back over, to
/// start when the machine is about as quiet as it gets (`cpu_low`).
pub const LOW_WINDOW: f64 = 120.0;

/// The cached reading if it is fresh, otherwise a new one. Call with the queue
/// lock held, so only one process refreshes at a time.
pub fn current(dir: &Path, max_age: f64, ours_kb: u64) -> MachineSample {
    let cached = read_cache(dir);
    if let Some(c) = &cached
        && crate::db::now() - c.ts <= max_age
    {
        return c.clone();
    }
    let s = measure(cached.as_ref(), ours_kb);
    write_cache(dir, &s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_from_an_older_version_reads_and_is_its_own_low() {
        let old =
            r#"{"ts":100.0,"cpu_busy":6.0,"cpu_inst":6.0,"ncpu":10,"mem_used_kb":1,"mem_total_kb":2,"mem_pressure":null,"recent_mem":[]}"#;
        let m: MachineSample = serde_json::from_str(old).unwrap();
        assert!(m.recent_cpu.is_empty());
        assert_eq!(m.cpu_low(), 6.0);
        assert!(m.near_cpu_low());
    }

    #[test]
    fn the_low_is_the_quietest_reading_of_the_window() {
        let m = MachineSample {
            ts: 1000.0,
            cpu_busy: 9.0,
            ncpu: 10,
            // The 1-core reading is older than the window.
            recent_cpu: vec![(850.0, 1.0), (900.0, 4.0), (950.0, 7.5), (1000.0, 9.0)],
            ..Default::default()
        };
        assert_eq!(m.cpu_low(), 4.0);
        assert!(!m.near_cpu_low(), "9 busy is far above the low of 4");
        let quiet = MachineSample { cpu_busy: 4.8, ..m.clone() };
        assert!(quiet.near_cpu_low(), "within one core of the low");
        let big = MachineSample { ncpu: 64, cpu_busy: 10.0, ..m };
        assert!(big.near_cpu_low(), "within a tenth of 64 cores of the low");
    }
}
