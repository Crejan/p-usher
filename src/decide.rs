//! The pure decision: given a thread, the CPU sets and the exemptions, what
//! (if anything) should happen to it.

use std::collections::HashSet;

use anyhow::{Result, bail};
use regex::Regex;

use crate::cpuset::CpuSet;
use crate::model::{Action, Policy, ThreadInfo};

/// Allowed set A, housekeeping set H and the CPU for `move`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sets {
    pub allowed: CpuSet,
    pub housekeeping: CpuSet,
    pub move_target: u32,
}

impl Sets {
    /// Fails when A is empty (there is no target) or when `move_target` is not
    /// in A.
    pub fn new(online: &CpuSet, allowed: &CpuSet, move_target: Option<u32>) -> Result<Self> {
        let Some(first) = allowed.first() else {
            bail!("allowed CPU set is empty (no isolated CPUs and no allowed_cpus)");
        };
        let move_target = move_target.unwrap_or(first);
        if !allowed.contains(move_target) {
            bail!("move_target {move_target} is not in the allowed CPUs {allowed}");
        }
        Ok(Self { allowed: allowed.clone(), housekeeping: online.difference(allowed), move_target })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Exemptions {
    pub uids: HashSet<u32>,
    pub gids: HashSet<u32>,
    pub comm: Vec<Regex>,
}

impl Exemptions {
    pub fn is_exempt(&self, t: &ThreadInfo) -> bool {
        self.uids.contains(&t.uid)
            || self.gids.contains(&t.gid)
            || t.groups.iter().any(|g| self.gids.contains(g))
            || self.comm.iter().any(|re| re.is_match(&t.comm))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not a candidate: kernel thread or not an RT policy.
    Ignore,
    Exempt,
    /// `SCHED_DEADLINE`: logged, never touched.
    Deadline,
    /// A candidate that stays off H.
    Compliant,
    Act {
        action: Action,
        cpus_after: CpuSet,
    },
}

pub fn decide(t: &ThreadInfo, sets: &Sets, exemptions: &Exemptions) -> Decision {
    if t.kthread {
        return Decision::Ignore;
    }
    if exemptions.is_exempt(t) {
        return Decision::Exempt;
    }
    if t.policy == Policy::Deadline {
        return Decision::Deadline;
    }
    if !t.policy.is_rt() {
        return Decision::Ignore;
    }
    if !t.cpus.intersects(&sets.housekeeping) {
        return Decision::Compliant;
    }
    let inside = t.cpus.intersection(&sets.allowed);
    if inside.is_empty() {
        Decision::Act { action: Action::Move, cpus_after: [sets.move_target].into_iter().collect() }
    } else {
        Decision::Act { action: Action::Restrict, cpus_after: inside }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpus(s: &str) -> CpuSet {
        s.parse().unwrap()
    }

    fn thread(policy: Policy, uid: u32, comm: &str, affinity: &str) -> ThreadInfo {
        ThreadInfo {
            tgid: 10,
            tid: 11,
            comm: comm.into(),
            uid,
            gid: uid,
            groups: vec![27],
            policy,
            rt_priority: 50,
            kthread: false,
            cpus: cpus(affinity),
        }
    }

    fn act(action: Action, after: &str) -> Decision {
        Decision::Act { action, cpus_after: cpus(after) }
    }

    #[test]
    fn decision_table() {
        use Policy::*;
        // online 0-15, A = 8-15, H = 0-7
        let sets = Sets::new(&cpus("0-15"), &cpus("8-15"), None).unwrap();
        let moved_to_12 = Sets::new(&cpus("0-15"), &cpus("8-15"), Some(12)).unwrap();
        let root_only = Exemptions { uids: [0].into(), ..Default::default() };
        let full =
            Exemptions { uids: [0, 2000].into(), gids: [500].into(), comm: vec![Regex::new("^ptp4l$").unwrap()] };
        let kthread = ThreadInfo { kthread: true, ..thread(Fifo, 0, "rcuc/0", "0") };
        let in_group = ThreadInfo { groups: vec![27, 500], ..thread(Fifo, 1000, "app", "0") };
        let primary_group = ThreadInfo { gid: 500, ..thread(Fifo, 1000, "app", "0") };

        #[rustfmt::skip]
        let table: Vec<(&str, ThreadInfo, &Sets, &Exemptions, Decision)> = vec![
            ("kernel thread",            kthread,                             &sets, &root_only, Decision::Ignore),
            ("SCHED_OTHER",              thread(Other, 1000, "app", "0-15"),  &sets, &root_only, Decision::Ignore),
            ("SCHED_BATCH",              thread(Batch, 1000, "app", "0"),     &sets, &root_only, Decision::Ignore),
            ("SCHED_DEADLINE",           thread(Deadline, 1000, "dl", "0-15"),&sets, &root_only, Decision::Deadline),
            ("root is exempt",           thread(Fifo, 0, "app", "0"),         &sets, &root_only, Decision::Exempt),
            ("exempt user",              thread(Fifo, 2000, "app", "0"),      &sets, &full,      Decision::Exempt),
            ("exempt supplementary grp", in_group,                            &sets, &full,      Decision::Exempt),
            ("exempt primary group",     primary_group,                       &sets, &full,      Decision::Exempt),
            ("exempt comm",              thread(Fifo, 1000, "ptp4l", "0"),    &sets, &full,      Decision::Exempt),
            ("comm regex is anchored",   thread(Fifo, 1000, "ptp4l2", "0"),   &sets, &full,      act(Action::Move, "8")),
            ("FIFO inside A",            thread(Fifo, 1000, "app", "8-15"),   &sets, &root_only, Decision::Compliant),
            ("RR on one allowed CPU",    thread(Rr, 1000, "app", "9"),        &sets, &root_only, Decision::Compliant),
            ("FIFO all CPUs: restrict",  thread(Fifo, 1000, "app", "0-15"),   &sets, &root_only, act(Action::Restrict, "8-15")),
            ("RR partial: restrict",     thread(Rr, 1000, "app", "6-9"),      &sets, &root_only, act(Action::Restrict, "8-9")),
            ("FIFO only H: move",        thread(Fifo, 1000, "app", "0-3"),    &sets, &root_only, act(Action::Move, "8")),
            ("move_target override",     thread(Fifo, 1000, "app", "2"),      &moved_to_12, &root_only, act(Action::Move, "12")),
            ("restrict ignores target",  thread(Fifo, 1000, "app", "0,9"),    &moved_to_12, &root_only, act(Action::Restrict, "9")),
        ];
        for (name, t, sets, ex, want) in table {
            assert_eq!(decide(&t, sets, ex), want, "{name}");
        }
    }

    #[test]
    fn sets_validation() {
        let s = Sets::new(&cpus("0-7"), &cpus("4-7"), None).unwrap();
        assert_eq!(s.housekeeping, cpus("0-3"));
        assert_eq!(s.move_target, 4);
        assert!(Sets::new(&cpus("0-7"), &CpuSet::new(), None).is_err());
        assert!(Sets::new(&cpus("0-7"), &cpus("4-7"), Some(2)).is_err());
    }
}
