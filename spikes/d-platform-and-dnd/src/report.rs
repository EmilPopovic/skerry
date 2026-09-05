// SPDX-License-Identifier: Apache-2.0
//! A check is a name, an outcome and the evidence for it. Nothing here decides
//! anything; it exists so that a run of this spike prints a matrix that can be
//! pasted into the README without being retyped, and so that a check which was
//! never reached is visibly distinct from one that passed.

use std::fmt::Write as _;

#[derive(Clone, Copy, PartialEq)]
pub enum Outcome {
    Pass,
    Fail,
    /// The protocol behaved as specified and the peer still did not do what a
    /// user would expect. The bar admits this outcome, but only with a fallback
    /// written next to it.
    Limitation,
    /// Reached, and the answer depends on something this machine does not have.
    Owed,
}

impl Outcome {
    fn tag(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Limitation => "LIMIT",
            Outcome::Owed => "OWED",
        }
    }
}

pub struct Check {
    pub name: String,
    pub outcome: Outcome,
    pub evidence: String,
}

#[derive(Default)]
pub struct Report {
    checks: Vec<Check>,
}

impl Report {
    pub fn record(&mut self, name: impl Into<String>, outcome: Outcome, evidence: impl Into<String>) {
        let c = Check { name: name.into(), outcome, evidence: evidence.into() };
        println!("  {:<5} {:<22} {}", c.outcome.tag(), c.name, c.evidence);
        self.checks.push(c);
    }

    pub fn pass(&mut self, name: impl Into<String>, evidence: impl Into<String>) {
        self.record(name, Outcome::Pass, evidence)
    }
    pub fn fail(&mut self, name: impl Into<String>, evidence: impl Into<String>) {
        self.record(name, Outcome::Fail, evidence)
    }
    pub fn owed(&mut self, name: impl Into<String>, evidence: impl Into<String>) {
        self.record(name, Outcome::Owed, evidence)
    }
    pub fn limitation(&mut self, name: impl Into<String>, evidence: impl Into<String>) {
        self.record(name, Outcome::Limitation, evidence)
    }

    pub fn summary(&self) -> String {
        let mut s = String::new();
        let n = |o: Outcome| self.checks.iter().filter(|c| c.outcome == o).count();
        let _ = write!(
            s,
            "{} pass, {} fail, {} limitation, {} owed, of {} checks",
            n(Outcome::Pass),
            n(Outcome::Fail),
            n(Outcome::Limitation),
            n(Outcome::Owed),
            self.checks.len()
        );
        s
    }

    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.outcome == Outcome::Fail)
    }
}

/// Load average, printed beside every latency because
/// [12-measurement.md](../../../docs/12-measurement.md) was written after a
/// contaminated run cost a day.
pub fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::NAN)
}

/// p50 and p99 of a sample set, because one median is not a result.
pub struct Samples(pub Vec<f64>);

impl Samples {
    pub fn describe(&self, unit: &str) -> String {
        if self.0.is_empty() {
            return "no samples".into();
        }
        let mut v = self.0.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[(((v.len() - 1) as f64) * p).round() as usize];
        format!(
            "p50 {:.3}{unit} p99 {:.3}{unit} max {:.3}{unit} n={}",
            q(0.50),
            q(0.99),
            v[v.len() - 1],
            v.len()
        )
    }
}
