//! Fakes shared by the integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use p_usher::accounts::StaticAccounts;
use p_usher::decide::Exemptions;
use p_usher::engine::{AffinitySetter, Env, NotifySwitches, SetError, Settings};
use p_usher::procfs::ProcFs;
use p_usher::throttle::FakeClock;
use p_usher::{CpuSet, Intervention, Mode, Notifier};

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Fixture tree with A = 2-3 (isolated), H = 0-1.
pub fn fixture_procfs() -> ProcFs {
    ProcFs::new(fixtures().join("proc"), fixtures().join("sys"))
}

/// Fixture tree whose `isolated` is empty.
pub fn fixture_procfs_no_isolation() -> ProcFs {
    ProcFs::new(fixtures().join("proc"), fixtures().join("sys-noisol"))
}

pub fn cpus(s: &str) -> CpuSet {
    s.parse().unwrap()
}

pub fn settings(mode: Mode) -> Settings {
    Settings {
        host: "rt-host".into(),
        mode,
        allowed_override: None,
        move_target: None,
        exemptions: Exemptions { uids: [0].into(), ..Default::default() },
        notify: NotifySwitches::default(),
        notify_throttle: Duration::from_secs(3600),
        backoff_initial: Duration::from_secs(10),
        backoff_max: Duration::from_secs(600),
    }
}

pub fn accounts() -> StaticAccounts {
    StaticAccounts {
        users: [(1000, "alice".into()), (1001, "dora".into()), (1002, "ptp".into()), (1003, "suid".into())].into(),
        groups: Default::default(),
    }
}

/// Records `sched_setaffinity` calls; fails them with `errno` if set.
#[derive(Clone, Default)]
pub struct FakeSetter {
    pub calls: Arc<Mutex<Vec<(i32, CpuSet)>>>,
    pub errno: Option<i32>,
}

impl AffinitySetter for FakeSetter {
    fn set_affinity(&mut self, tid: i32, cpus: &CpuSet) -> Result<(), SetError> {
        self.calls.lock().unwrap().push((tid, cpus.clone()));
        match self.errno {
            Some(libc::ESRCH) => Err(SetError::Gone),
            Some(errno) => Err(SetError::Failed(std::io::Error::from_raw_os_error(errno))),
            None => Ok(()),
        }
    }
}

/// Records what it is notified of.
#[derive(Clone)]
pub struct Recorder {
    pub events: Arc<Mutex<Vec<Intervention>>>,
    pub allowed: Arc<Mutex<Option<CpuSet>>>,
    pub throttled: bool,
}

impl Recorder {
    pub fn new(throttled: bool) -> Self {
        Self { events: Default::default(), allowed: Default::default(), throttled }
    }

    pub fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl Notifier for Recorder {
    fn notify(&mut self, ev: &Intervention) -> anyhow::Result<()> {
        self.events.lock().unwrap().push(ev.clone());
        Ok(())
    }

    fn throttled(&self) -> bool {
        self.throttled
    }

    fn tick(&mut self, _now: SystemTime, allowed: &CpuSet) -> anyhow::Result<()> {
        *self.allowed.lock().unwrap() = Some(allowed.clone());
        Ok(())
    }
}

pub fn env(procfs: ProcFs, setter: FakeSetter, clock: &FakeClock, notifiers: Vec<Box<dyn Notifier>>) -> Env {
    Env { procfs, setter: Box::new(setter), clock: Arc::new(clock.clone()), accounts: Box::new(accounts()), notifiers }
}

pub fn clock() -> FakeClock {
    FakeClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
}
