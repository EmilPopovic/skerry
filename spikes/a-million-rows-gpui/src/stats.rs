//! Percentiles, process footprint and idle CPU.
//!
//! The rule this file exists to enforce: a number is reported with its sample
//! count, and a number that could not be measured is reported as absent rather
//! than as zero.

pub struct Samples {
    pub label: &'static str,
    values: Vec<f32>,
}

impl Samples {
    pub fn new(label: &'static str) -> Self {
        Self { label, values: Vec::with_capacity(2048) }
    }

    pub fn push(&mut self, ms: f32) {
        self.values.push(ms);
    }

    pub fn extend(&mut self, ms: impl IntoIterator<Item = f32>) {
        self.values.extend(ms);
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Nearest-rank percentile on the sorted sample. No interpolation: with a
    /// few hundred samples, interpolating invents precision.
    pub fn percentile(&self, p: f32) -> f32 {
        if self.values.is_empty() {
            return f32::NAN;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f32::total_cmp);
        let rank = ((p / 100.0) * sorted.len() as f32).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }

    pub fn max(&self) -> f32 {
        self.values.iter().copied().fold(f32::NAN, f32::max)
    }

    pub fn mean(&self) -> f32 {
        self.values.iter().sum::<f32>() / self.values.len() as f32
    }

    /// `p50 / p99 / max` over n, formatted once so every table row matches.
    pub fn row(&self) -> String {
        if self.is_empty() {
            return format!("{:<10} no samples", self.label);
        }
        format!(
            "{:<10} p50 {:>6.3}  p99 {:>6.3}  max {:>7.3}  mean {:>6.3}   n={}",
            self.label,
            self.percentile(50.0),
            self.percentile(99.0),
            self.max(),
            self.mean(),
            self.len(),
        )
    }
}

/// Peak resident set size in bytes, from `VmHWM`. Peak rather than current,
/// because the budget in docs/09-performance-budgets.md says "Peak".
pub fn peak_rss() -> Option<u64> {
    field("/proc/self/status", "VmHWM:").map(|kb| kb * 1024)
}

pub fn current_rss() -> Option<u64> {
    field("/proc/self/status", "VmRSS:").map(|kb| kb * 1024)
}

fn field(path: &str, name: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

/// Total CPU time this process has used, in seconds, from `utime + stime` in
/// `/proc/self/stat`. Includes every thread, which is the point: a job pool
/// waking up at idle is exactly the failure the bar forbids.
pub fn cpu_seconds() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The second field is the comm name in parentheses and may contain spaces,
    // so fields are counted from after the closing parenthesis. proc(5).
    let tail = &text[text.rfind(')')? + 2..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let hz = 100.0; // CONFIG_HZ / USER_HZ is 100 on every Linux target we ship to.
    Some((utime + stime) as f64 / hz)
}
