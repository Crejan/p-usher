//! Whole scans against the fixture tree with a fake setter and a fake clock:
//! enforce vs. report, empty A, backoff and notification throttling.

mod common;

use std::time::Duration;

use common::*;
use p_usher::engine::Engine;
use p_usher::{Action, CpuSet, Intervention, Mode};

/// (tid, action, cpus_after) of each event, for comparing modes.
fn summary(events: &[Intervention]) -> Vec<(i32, Action, String)> {
    events.iter().map(|e| (e.tid, e.action, e.cpus_after.to_string())).collect()
}

fn expected() -> Vec<(i32, Action, String)> {
    // A = 2-3. 101: 0-3 -> restrict; 102: 1 -> move to 2; 500 ptp4l (not
    // exempt by default): 0 -> move; 600: real uid 1003 is not exempt even
    // though its effective uid is 0. 2 (kthread), 103 (inside A), 300 (root)
    // and 400 (deadline) are untouched.
    vec![
        (101, Action::Restrict, "2-3".into()),
        (102, Action::Move, "2".into()),
        (500, Action::Move, "2".into()),
        (600, Action::Move, "2".into()),
    ]
}

#[test]
fn enforce_applies_decisions() {
    let clock = clock();
    let setter = FakeSetter::default();
    let rec = Recorder::new(false);
    let mut engine = Engine::new(
        settings(Mode::Enforce),
        env(fixture_procfs(), setter.clone(), &clock, vec![Box::new(rec.clone())]),
    );

    let events = engine.tick().unwrap();
    assert_eq!(summary(&events), expected());
    assert!(events.iter().all(|e| e.enforced));
    let calls: Vec<(i32, String)> = setter.calls.lock().unwrap().iter().map(|(t, c)| (*t, c.to_string())).collect();
    assert_eq!(calls, vec![(101, "2-3".into()), (102, "2".into()), (500, "2".into()), (600, "2".into())]);

    let e = &events[0];
    assert_eq!((e.host.as_str(), e.user.as_str(), e.tgid, e.comm.as_str()), ("rt-host", "alice", 100, "rt worker) 1"));
    assert_eq!(e.exe.as_deref(), Some(std::path::Path::new("/usr/bin/rtapp")));
    assert_eq!(e.gids, vec![27, 1000]);
    assert_eq!(e.cpus_before, cpus("0-3"));
    assert_eq!(rec.count(), 4);
    assert_eq!(*rec.allowed.lock().unwrap(), Some(cpus("2-3")));
}

#[test]
fn report_mode_decides_the_same_but_never_sets_affinity() {
    let clock = clock();
    let setter = FakeSetter::default();
    let rec = Recorder::new(false);
    let mut engine =
        Engine::new(settings(Mode::Report), env(fixture_procfs(), setter.clone(), &clock, vec![Box::new(rec.clone())]));

    let events = engine.tick().unwrap();
    assert_eq!(summary(&events), expected());
    assert!(events.iter().all(|e| !e.enforced));
    assert!(setter.calls.lock().unwrap().is_empty());
    assert_eq!(rec.count(), 4);

    // Nothing changed on the host, so nothing new to report.
    clock.advance(Duration::from_secs(1));
    assert!(engine.tick().unwrap().is_empty());
    assert!(setter.calls.lock().unwrap().is_empty());
}

#[test]
fn empty_allowed_set_evaluates_nothing() {
    for mode in [Mode::Enforce, Mode::Report] {
        let clock = clock();
        let setter = FakeSetter::default();
        let rec = Recorder::new(false);
        let mut engine = Engine::new(
            settings(mode),
            env(fixture_procfs_no_isolation(), setter.clone(), &clock, vec![Box::new(rec.clone())]),
        );
        assert!(engine.tick().unwrap().is_empty(), "{mode:?}");
        assert!(setter.calls.lock().unwrap().is_empty());
        assert_eq!(rec.count(), 0);
        // Exported as p_usher_allowed_cpus 0.
        assert_eq!(*rec.allowed.lock().unwrap(), Some(CpuSet::new()));
        assert_eq!(engine.housekeeping(), cpus("0-3"));
    }
}

#[test]
fn allowed_cpus_override_and_move_target() {
    let clock = clock();
    let mut s = settings(Mode::Enforce);
    s.allowed_override = Some(cpus("3"));
    s.move_target = Some(3);
    let mut engine = Engine::new(s, env(fixture_procfs_no_isolation(), FakeSetter::default(), &clock, vec![]));
    let got = summary(&engine.tick().unwrap());
    assert_eq!(got[0], (101, Action::Restrict, "3".into()));
    assert_eq!(got[1], (102, Action::Move, "3".into()));
    // 103 (2-3) now also touches H = 0-2.
    assert!(got.contains(&(103, Action::Restrict, "3".into())));
}

#[test]
fn move_target_outside_allowed_set_evaluates_nothing() {
    let clock = clock();
    let mut s = settings(Mode::Enforce);
    s.move_target = Some(0);
    let mut engine = Engine::new(s, env(fixture_procfs(), FakeSetter::default(), &clock, vec![]));
    assert!(engine.tick().unwrap().is_empty());
}

#[test]
fn failed_setaffinity_backs_off_per_tid() {
    let clock = clock();
    let setter = FakeSetter { errno: Some(libc::EINVAL), ..Default::default() };
    let rec = Recorder::new(false);
    let mut engine = Engine::new(
        settings(Mode::Enforce),
        env(fixture_procfs(), setter.clone(), &clock, vec![Box::new(rec.clone())]),
    );
    let attempts = || setter.calls.lock().unwrap().len();

    let events = engine.tick().unwrap();
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(|e| e.action == Action::Failed && e.enforced));
    assert_eq!(events[0].cpus_after, cpus("2-3"));
    assert_eq!(attempts(), 4);

    // Within the first 10 s: no retries.
    for _ in 0..9 {
        clock.advance(Duration::from_secs(1));
        assert!(engine.tick().unwrap().is_empty());
    }
    assert_eq!(attempts(), 4);

    // 10 s: retried, fails again, next wait 20 s.
    clock.advance(Duration::from_secs(1));
    assert_eq!(engine.tick().unwrap().len(), 4);
    assert_eq!(attempts(), 8);
    clock.advance(Duration::from_secs(19));
    assert!(engine.tick().unwrap().is_empty());
    clock.advance(Duration::from_secs(1));
    assert_eq!(engine.tick().unwrap().len(), 4);
    assert_eq!(rec.count(), 12);
}

#[test]
fn vanished_thread_is_ignored() {
    let clock = clock();
    let setter = FakeSetter { errno: Some(libc::ESRCH), ..Default::default() };
    let mut engine = Engine::new(settings(Mode::Enforce), env(fixture_procfs(), setter.clone(), &clock, vec![]));
    assert!(engine.tick().unwrap().is_empty());
    // No backoff: tried again next tick.
    clock.advance(Duration::from_secs(1));
    engine.tick().unwrap();
    assert_eq!(setter.calls.lock().unwrap().len(), 8);
}

#[test]
fn repinning_thread_is_enforced_each_time_but_notified_once_per_throttle() {
    // The fixture never changes, which is what a thread that re-pins itself
    // onto H after every intervention looks like.
    let clock = clock();
    let setter = FakeSetter::default();
    let mail = Recorder::new(true);
    let metrics = Recorder::new(false);
    let mut engine = Engine::new(
        settings(Mode::Enforce),
        env(fixture_procfs(), setter.clone(), &clock, vec![Box::new(mail.clone()), Box::new(metrics.clone())]),
    );

    for _ in 0..5 {
        assert_eq!(engine.tick().unwrap().len(), 4);
        clock.advance(Duration::from_secs(1));
    }
    assert_eq!(setter.calls.lock().unwrap().len(), 20);
    assert_eq!(metrics.count(), 20);
    // Keys (user, comm, action): (alice, "rt worker) 1", restrict),
    // (alice, spin, move), (ptp, ptp4l, move), (suid, suidrt, move).
    assert_eq!(mail.count(), 4);

    clock.advance(Duration::from_secs(3600));
    engine.tick().unwrap();
    assert_eq!(mail.count(), 8);
    assert_eq!(metrics.count(), 24);
}

#[test]
fn notify_switches_filter_notifiers_not_enforcement() {
    let clock = clock();
    let setter = FakeSetter::default();
    let rec = Recorder::new(false);
    let mut s = settings(Mode::Enforce);
    s.notify.r#move = false;
    let mut engine = Engine::new(s, env(fixture_procfs(), setter.clone(), &clock, vec![Box::new(rec.clone())]));
    assert_eq!(engine.tick().unwrap().len(), 4);
    assert_eq!(setter.calls.lock().unwrap().len(), 4);
    let notified: Vec<Action> = rec.events.lock().unwrap().iter().map(|e| e.action).collect();
    assert_eq!(notified, vec![Action::Restrict]);
}

#[test]
fn exemptions_from_settings() {
    let clock = clock();
    let mut s = settings(Mode::Enforce);
    s.exemptions.comm.push(regex::Regex::new("^ptp4l$").unwrap());
    s.exemptions.uids.insert(1003);
    let mut engine = Engine::new(s, env(fixture_procfs(), FakeSetter::default(), &clock, vec![]));
    let tids: Vec<i32> = engine.tick().unwrap().iter().map(|e| e.tid).collect();
    assert_eq!(tids, vec![101, 102]);
}
