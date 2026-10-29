//! Where interventions go besides stderr.

pub mod email;
pub mod prometheus;

use std::time::SystemTime;

use crate::cpuset::CpuSet;
use crate::model::Intervention;

pub trait Notifier: Send {
    fn notify(&mut self, ev: &Intervention) -> anyhow::Result<()>;

    /// Whether `notify_throttle` applies. Notifiers addressed to people
    /// (email) are throttled; metrics see every intervention.
    fn throttled(&self) -> bool {
        true
    }

    /// Called once per scan, also when nothing happened: expiry, gauges.
    fn tick(&mut self, _now: SystemTime, _allowed: &CpuSet) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called once before the daemon exits.
    fn shutdown(&mut self) {}
}
