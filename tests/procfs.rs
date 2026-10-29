//! `/proc` and `/sys` parsing against the fixture tree in `tests/fixtures`.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{cpus, fixture_procfs, fixture_procfs_no_isolation, fixtures};
use p_usher::Policy;
use p_usher::procfs::{PF_KTHREAD, parse_sched, parse_stat, parse_status};

fn read(rel: &str) -> String {
    fs::read_to_string(fixtures().join("proc").join(rel)).unwrap()
}

#[test]
fn stat_with_parentheses_and_spaces_in_comm() {
    let s = parse_stat(&read("100/task/101/stat")).unwrap();
    assert_eq!(s.pid, 101);
    assert_eq!(s.comm, "rt worker) 1");
    assert_eq!(s.policy, Policy::Fifo);
    assert_eq!(s.rt_priority, 50);
    assert_eq!(s.flags & PF_KTHREAD, 0);
}

#[test]
fn stat_of_kernel_thread() {
    let s = parse_stat(&read("2/task/2/stat")).unwrap();
    assert_eq!(s.comm, "rcuc/0");
    assert_ne!(s.flags & PF_KTHREAD, 0);
}

#[test]
fn stat_policies() {
    for (rel, policy) in [
        ("100/task/100/stat", Policy::Other),
        ("100/task/102/stat", Policy::Rr),
        ("400/task/400/stat", Policy::Deadline),
    ] {
        assert_eq!(parse_stat(&read(rel)).unwrap().policy, policy, "{rel}");
    }
}

#[test]
fn stat_rejects_truncated_input() {
    assert!(parse_stat("12 (x) S 1 2 3").is_err());
    assert!(parse_stat("no parens").is_err());
}

#[test]
fn status_real_ids_groups_and_affinity() {
    let s = parse_status(&read("100/task/101/status")).unwrap();
    assert_eq!((s.tgid, s.pid, s.uid, s.gid), (100, 101, 1000, 1000));
    assert_eq!(s.groups, vec![27, 1000]);
    assert_eq!(s.cpus_allowed, cpus("0-3"));

    // setuid binary: real uid 1003, effective uid 0
    let s = parse_status(&read("600/task/600/status")).unwrap();
    assert_eq!(s.uid, 1003);
    assert!(s.groups.is_empty());
}

#[test]
fn status_requires_affinity() {
    assert!(parse_status("Tgid:\t1\nPid:\t1\nUid:\t0 0 0 0\nGid:\t0 0 0 0\n").is_err());
}

#[test]
fn sched_policy_and_prio() {
    let s = parse_sched(&read("100/task/101/sched")).unwrap();
    assert_eq!(s.policy, Policy::Fifo);
    assert_eq!(s.prio, 49);
    assert!(parse_sched("x (1, #threads: 1)\n---\n").is_err());
}

#[test]
fn sysfs_cpu_lists() {
    let p = fixture_procfs();
    assert_eq!(p.online_cpus().unwrap(), cpus("0-3"));
    assert_eq!(p.isolated_cpus().unwrap(), cpus("2-3"));
    assert!(fixture_procfs_no_isolation().isolated_cpus().unwrap().is_empty());
}

#[test]
fn scan_returns_rt_and_deadline_tasks_only() {
    let threads = fixture_procfs().rt_threads().unwrap();
    let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
    // 100 (SCHED_OTHER) is left out; the kernel thread is kept for decide().
    assert_eq!(tids, vec![2, 101, 102, 103, 300, 400, 500, 600]);

    let t = threads.iter().find(|t| t.tid == 102).unwrap();
    assert_eq!((t.tgid, t.uid, t.policy, t.rt_priority), (100, 1000, Policy::Rr, 10));
    assert_eq!(t.cpus, cpus("1"));
    assert!(threads.iter().find(|t| t.tid == 2).unwrap().kthread);
}

#[test]
fn exe_link() {
    let p = fixture_procfs();
    assert_eq!(p.exe(100), Some(PathBuf::from("/usr/bin/rtapp")));
    assert_eq!(p.exe(300), None);
}
