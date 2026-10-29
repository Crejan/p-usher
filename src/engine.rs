//! One scan: read the CPU sets and the RT threads, decide, apply (or not),
//! log and notify.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::accounts::Accounts;
use crate::cpuset::CpuSet;
use crate::decide::{Decision, Exemptions, Sets, decide};
use crate::model::{Action, Intervention, Mode, ThreadInfo};
use crate::notify::Notifier;
use crate::procfs::ProcFs;
use crate::throttle::{Backoff, Clock, Throttle};

/// `[notify]`: which actions reach the notifiers (stderr gets all of them).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifySwitches {
    pub restrict: bool,
    pub r#move: bool,
    pub failed: bool,
}

impl Default for NotifySwitches {
    fn default() -> Self {
        Self { restrict: true, r#move: true, failed: true }
    }
}

impl NotifySwitches {
    pub fn enabled(&self, action: Action) -> bool {
        match action {
            Action::Restrict => self.restrict,
            Action::Move => self.r#move,
            Action::Failed => self.failed,
        }
    }
}

#[derive(Debug)]
pub enum SetError {
    /// `ESRCH`: the thread exited.
    Gone,
    Failed(io::Error),
}

pub trait AffinitySetter: Send {
    fn set_affinity(&mut self, tid: i32, cpus: &CpuSet) -> Result<(), SetError>;
}

/// The real `sched_setaffinity`.
pub struct SystemSetter;

impl AffinitySetter for SystemSetter {
    fn set_affinity(&mut self, tid: i32, cpus: &CpuSet) -> Result<(), SetError> {
        crate::sys::set_affinity(tid, cpus).map_err(|e| match e.raw_os_error() {
            Some(libc::ESRCH) => SetError::Gone,
            _ => SetError::Failed(e),
        })
    }
}

pub struct Settings {
    pub host: String,
    pub mode: Mode,
    /// `allowed_cpus`; `None` reads `/sys/devices/system/cpu/isolated`.
    pub allowed_override: Option<CpuSet>,
    pub move_target: Option<u32>,
    pub exemptions: Exemptions,
    pub notify: NotifySwitches,
    pub notify_throttle: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

/// What the engine talks to; swapped for fixtures and fakes in tests.
pub struct Env {
    pub procfs: ProcFs,
    pub setter: Box<dyn AffinitySetter>,
    pub clock: Arc<dyn Clock>,
    pub accounts: Box<dyn Accounts>,
    pub notifiers: Vec<Box<dyn Notifier>>,
}

pub struct Engine {
    settings: Settings,
    env: Env,
    throttle: Throttle<(String, String, Action)>,
    backoff: Backoff,
    /// Last seen (online, allowed); A changes are logged once.
    cpus: Option<(CpuSet, CpuSet)>,
    sets: Option<Sets>,
    /// Report mode: affinity last reported per tid, so an unchanged thread
    /// is reported once, just as enforce would act once.
    reported: HashMap<i32, CpuSet>,
    deadline_logged: HashSet<i32>,
    user_names: HashMap<u32, String>,
}

impl Engine {
    pub fn new(settings: Settings, env: Env) -> Self {
        Self {
            throttle: Throttle::new(settings.notify_throttle),
            backoff: Backoff::new(settings.backoff_initial, settings.backoff_max),
            settings,
            env,
            cpus: None,
            sets: None,
            reported: HashMap::new(),
            deadline_logged: HashSet::new(),
            user_names: HashMap::new(),
        }
    }

    /// Re-reads online CPUs and A. `None` while A is empty or invalid.
    pub fn refresh_sets(&mut self) -> Result<Option<&Sets>> {
        let online = self.env.procfs.online_cpus()?;
        let wanted = match &self.settings.allowed_override {
            Some(a) => a.clone(),
            None => self.env.procfs.isolated_cpus()?,
        };
        let allowed = wanted.intersection(&online);
        if self.cpus.as_ref() != Some(&(online.clone(), allowed.clone())) {
            if allowed != wanted {
                log_warn!("allowed CPUs {wanted} include offline CPUs; using {allowed}");
            }
            self.sets = match Sets::new(&online, &allowed, self.settings.move_target) {
                Ok(sets) => {
                    log_info!(
                        "allowed CPUs {}, housekeeping CPUs {}, move target {}, mode {:?}",
                        sets.allowed,
                        sets.housekeeping,
                        sets.move_target,
                        self.settings.mode
                    );
                    Some(sets)
                }
                Err(e) => {
                    log_error!("{e:#}; no thread is evaluated");
                    None
                }
            };
            self.cpus = Some((online, allowed));
        }
        Ok(self.sets.as_ref())
    }

    /// Online CPUs minus A, as of the last refresh (all online CPUs when A is
    /// empty).
    pub fn housekeeping(&self) -> CpuSet {
        self.cpus.as_ref().map(|(online, allowed)| online.difference(allowed)).unwrap_or_default()
    }

    /// One full scan of `/proc`.
    pub fn tick(&mut self) -> Result<Vec<Intervention>> {
        self.refresh_sets()?;
        let threads = if self.sets.is_some() { self.env.procfs.rt_threads()? } else { Vec::new() };
        let events = self.process(threads);
        let (now, allowed) = (self.env.clock.wall(), self.sets.as_ref().map(|s| s.allowed.clone()).unwrap_or_default());
        for n in &mut self.env.notifiers {
            if let Err(e) = n.tick(now, &allowed) {
                log_warn!("notifier: {e:#}");
            }
        }
        Ok(events)
    }

    /// Evaluates `threads` as one complete scan: per-tid state of threads not
    /// in the list is dropped. Does nothing without valid sets.
    pub fn process(&mut self, threads: Vec<ThreadInfo>) -> Vec<Intervention> {
        let Some(sets) = self.sets.clone() else { return Vec::new() };
        let now = self.env.clock.now();
        let mut events = Vec::new();
        let live: HashSet<i32> = threads.iter().map(|t| t.tid).collect();
        for t in &threads {
            match decide(t, &sets, &self.settings.exemptions) {
                Decision::Act { action, cpus_after } => {
                    if let Some(ev) = self.act(t, action, cpus_after, now) {
                        events.push(ev);
                    }
                }
                Decision::Deadline => {
                    if self.deadline_logged.insert(t.tid) && t.cpus.intersects(&sets.housekeeping) {
                        log_info!(
                            "leaving SCHED_DEADLINE thread {} (tid {}, uid {}) alone on CPUs {}",
                            t.comm,
                            t.tid,
                            t.uid,
                            t.cpus
                        );
                    }
                }
                Decision::Ignore | Decision::Exempt | Decision::Compliant => {
                    self.reported.remove(&t.tid);
                    self.backoff.succeeded(t.tid);
                }
            }
        }
        self.backoff.retain(&live);
        self.reported.retain(|tid, _| live.contains(tid));
        self.deadline_logged.retain(|tid| live.contains(tid));
        events
    }

    fn act(&mut self, t: &ThreadInfo, action: Action, after: CpuSet, now: std::time::Instant) -> Option<Intervention> {
        match self.settings.mode {
            Mode::Report => {
                if self.reported.get(&t.tid) == Some(&t.cpus) {
                    return None;
                }
                self.reported.insert(t.tid, t.cpus.clone());
                Some(self.emit(t, action, after, false))
            }
            Mode::Enforce => {
                if self.backoff.blocked(t.tid, now) {
                    return None;
                }
                match self.env.setter.set_affinity(t.tid, &after) {
                    Ok(()) => {
                        self.backoff.succeeded(t.tid);
                        Some(self.emit(t, action, after, true))
                    }
                    Err(SetError::Gone) => None,
                    Err(SetError::Failed(e)) => {
                        log_warn!("sched_setaffinity({}, {after}): {e}", t.tid);
                        self.backoff.failed(t.tid, now);
                        Some(self.emit(t, Action::Failed, after, true))
                    }
                }
            }
        }
    }

    fn emit(&mut self, t: &ThreadInfo, action: Action, after: CpuSet, enforced: bool) -> Intervention {
        let accounts = &self.env.accounts;
        let user = self
            .user_names
            .entry(t.uid)
            .or_insert_with(|| accounts.user_name(t.uid).unwrap_or_else(|| t.uid.to_string()))
            .clone();
        let mut gids: Vec<u32> = std::iter::once(t.gid).chain(t.groups.iter().copied()).collect();
        gids.sort_unstable();
        gids.dedup();
        let ev = Intervention {
            host: self.settings.host.clone(),
            at: self.env.clock.wall(),
            tgid: t.tgid,
            tid: t.tid,
            comm: t.comm.clone(),
            exe: self.env.procfs.exe(t.tgid),
            uid: t.uid,
            user,
            gids,
            policy: t.policy,
            rt_priority: t.rt_priority,
            cpus_before: t.cpus.clone(),
            cpus_after: after,
            action,
            enforced,
        };
        log_info!(
            "{}{} tid {} (pid {}, {:?}, user {}, {} prio {}): CPUs {} -> {}",
            action,
            if enforced { "" } else { " (report)" },
            ev.tid,
            ev.tgid,
            ev.comm,
            ev.user,
            ev.policy,
            ev.rt_priority,
            ev.cpus_before,
            ev.cpus_after
        );
        if self.settings.notify.enabled(action) {
            let fresh = self.throttle.allow((ev.user.clone(), ev.comm.clone(), action), self.env.clock.now());
            for n in &mut self.env.notifiers {
                if n.throttled() && !fresh {
                    continue;
                }
                if let Err(e) = n.notify(&ev) {
                    log_warn!("notifier: {e:#}");
                }
            }
        }
        ev
    }

    pub fn shutdown(&mut self) {
        for n in &mut self.env.notifiers {
            n.shutdown();
        }
    }
}
