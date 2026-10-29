//! Real kernel: a SCHED_FIFO thread pinned to a housekeeping CPU is moved.
//! Needs Linux, CAP_SYS_NICE and at least two online CPUs; skipped otherwise.
#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{Recorder, settings};
use p_usher::accounts::SystemAccounts;
use p_usher::decide::Exemptions;
use p_usher::engine::{Engine, Env, SystemSetter};
use p_usher::procfs::ProcFs;
use p_usher::throttle::SystemClock;
use p_usher::{Action, CpuSet, Mode, Policy, sys};

const CAP_SYS_NICE: u32 = 23;

fn has_cap_sys_nice() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_SYS_NICE) != 0)
}

fn one(cpu: u32) -> CpuSet {
    [cpu].into_iter().collect()
}

#[test]
fn fifo_thread_on_housekeeping_cpu_is_moved() {
    if !has_cap_sys_nice() {
        eprintln!("skipped: needs CAP_SYS_NICE");
        return;
    }
    let procfs = ProcFs::system();
    let online = procfs.online_cpus().unwrap();
    if online.len() < 2 {
        eprintln!("skipped: needs two online CPUs");
        return;
    }
    let housekeeping_cpu = online.first().unwrap();
    let allowed_cpu = online.iter().last().unwrap();

    // A FIFO thread that sleeps in its loop, pinned to H.
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            sys::set_affinity(0, &one(housekeeping_cpu)).unwrap();
            sys::set_fifo(0, 1).unwrap();
            tx.send(sys::gettid()).unwrap();
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let tid = rx.recv().unwrap();
    let tgid = std::process::id() as i32;
    let t = procfs.read_thread(tgid, tid).unwrap().expect("thread is SCHED_FIFO");
    assert_eq!(t.policy, Policy::Fifo);
    assert_eq!(t.cpus, one(housekeeping_cpu));

    // Root (as the test may well run) must not be exempt here.
    let mut s = settings(Mode::Enforce);
    s.allowed_override = Some(one(allowed_cpu));
    s.exemptions = Exemptions::default();
    let rec = Recorder::new(false);
    let mut engine = Engine::new(
        s,
        Env {
            procfs: procfs.clone(),
            setter: Box::new(SystemSetter),
            clock: Arc::new(SystemClock),
            accounts: Box::new(SystemAccounts),
            notifiers: vec![Box::new(rec.clone())],
        },
    );
    assert!(engine.refresh_sets().unwrap().is_some());
    // Only our own thread: never touch other RT threads on the test host.
    let events = engine.process(vec![t]);

    stop.store(true, Ordering::Relaxed);
    let after = sys::get_affinity(tid);
    worker.join().unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, Action::Move);
    assert!(events[0].enforced);
    assert_eq!(events[0].cpus_after, one(allowed_cpu));
    assert_eq!(after.unwrap(), one(allowed_cpu));
    assert_eq!(rec.count(), 1);
}
