//! Reference adapter: a node_exporter textfile, rewritten atomically.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::Notifier;
use crate::cpuset::CpuSet;
use crate::model::{Action, Intervention};

pub const FILE_NAME: &str = "p_usher.prom";

pub struct PrometheusTextfile {
    path: PathBuf,
    retain: Duration,
    counters: BTreeMap<(String, Action, bool), u64>,
    last: BTreeMap<(String, String, Action, bool), SystemTime>,
    allowed: Option<usize>,
    up: bool,
}

impl PrometheusTextfile {
    pub fn new(dir: &Path, retain: Duration) -> Self {
        Self {
            path: dir.join(FILE_NAME),
            retain,
            counters: BTreeMap::new(),
            last: BTreeMap::new(),
            allowed: None,
            up: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let header = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        header(
            &mut out,
            "p_usher_interventions_total",
            "counter",
            "Interventions on RT threads running on housekeeping CPUs.",
        );
        for ((user, action, enforced), n) in &self.counters {
            let _ = writeln!(
                out,
                "p_usher_interventions_total{{user=\"{}\",action=\"{action}\",enforced=\"{enforced}\"}} {n}",
                escape(user)
            );
        }
        header(
            &mut out,
            "p_usher_last_intervention_timestamp_seconds",
            "gauge",
            "Unix time of the last intervention per user, comm and action.",
        );
        for ((user, comm, action, enforced), at) in &self.last {
            let secs = at.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
            let _ = writeln!(
                out,
                "p_usher_last_intervention_timestamp_seconds{{user=\"{}\",comm=\"{}\",action=\"{action}\",enforced=\"{enforced}\"}} {secs:.3}",
                escape(user),
                escape(comm)
            );
        }
        header(&mut out, "p_usher_allowed_cpus", "gauge", "Number of CPUs RT threads are allowed on.");
        let _ = writeln!(out, "p_usher_allowed_cpus {}", self.allowed.unwrap_or(0));
        header(&mut out, "p_usher_up", "gauge", "1 while p-usher is running.");
        let _ = writeln!(out, "p_usher_up {}", u8::from(self.up));
        out
    }

    fn write(&self) -> Result<()> {
        let tmp = self.path.with_extension("prom.tmp");
        fs::write(&tmp, self.render()).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| format!("renaming to {}", self.path.display()))
    }
}

impl Notifier for PrometheusTextfile {
    fn notify(&mut self, ev: &Intervention) -> Result<()> {
        *self.counters.entry((ev.user.clone(), ev.action, ev.enforced)).or_default() += 1;
        self.last.insert((ev.user.clone(), ev.comm.clone(), ev.action, ev.enforced), ev.at);
        self.write()
    }

    fn throttled(&self) -> bool {
        false
    }

    fn tick(&mut self, now: SystemTime, allowed: &CpuSet) -> Result<()> {
        let before = self.last.len();
        let retain = self.retain;
        self.last.retain(|_, at| now.duration_since(*at).map_or(true, |age| age < retain));
        let count = Some(allowed.len());
        if self.last.len() == before && self.allowed == count {
            return Ok(());
        }
        self.allowed = count;
        self.write()
    }

    fn shutdown(&mut self) {
        self.up = false;
        if let Err(e) = self.write() {
            log_warn!("prometheus: {e:#}");
        }
    }
}

/// Label value escaping per the text exposition format.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}
