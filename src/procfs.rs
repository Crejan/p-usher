//! Reading tasks from `/proc` and CPU sets from `/sys`. Both roots are
//! configurable so the parsers run against fixture trees in tests.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cpuset::CpuSet;
use crate::model::{Policy, ThreadInfo};

/// `PF_KTHREAD` from `include/linux/sched.h`.
pub const PF_KTHREAD: u32 = 0x0020_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub pid: i32,
    pub comm: String,
    pub flags: u32,
    pub rt_priority: u32,
    pub policy: Policy,
}

/// Parses `/proc/<pid>/stat`. The comm may contain spaces and parentheses, so
/// it spans from the first `(` to the last `)`.
pub fn parse_stat(s: &str) -> Result<Stat> {
    let open = s.find('(').context("stat: missing '('")?;
    let close = s.rfind(')').context("stat: missing ')'")?;
    let pid = s[..open].trim().parse().context("stat: pid")?;
    let comm = s[open + 1..close].to_string();
    let rest: Vec<&str> = s[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state), so field n (1-based, man 5 proc) is rest[n - 3].
    let field =
        |n: usize| -> Result<&str> { rest.get(n - 3).copied().with_context(|| format!("stat: missing field {n}")) };
    Ok(Stat {
        pid,
        comm,
        flags: field(9)?.parse().context("stat: flags")?,
        rt_priority: field(40)?.parse().context("stat: rt_priority")?,
        policy: Policy::from_raw(field(41)?.parse().context("stat: policy")?),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub tgid: i32,
    pub pid: i32,
    /// Real uid (first of the four `Uid:` values).
    pub uid: u32,
    /// Real gid (first of the four `Gid:` values).
    pub gid: u32,
    pub groups: Vec<u32>,
    pub cpus_allowed: CpuSet,
}

/// Parses `/proc/<pid>/status`.
pub fn parse_status(s: &str) -> Result<Status> {
    let (mut tgid, mut pid, mut uid, mut gid, mut groups, mut cpus) = (None, None, None, None, None, None);
    for line in s.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        let first = || value.split_whitespace().next().unwrap_or_default();
        match key {
            "Tgid" => tgid = Some(value.parse().context("status: Tgid")?),
            "Pid" => pid = Some(value.parse().context("status: Pid")?),
            "Uid" => uid = Some(first().parse().context("status: Uid")?),
            "Gid" => gid = Some(first().parse().context("status: Gid")?),
            "Groups" => {
                groups = Some(
                    value
                        .split_whitespace()
                        .map(str::parse)
                        .collect::<Result<Vec<u32>, _>>()
                        .context("status: Groups")?,
                )
            }
            "Cpus_allowed_list" => cpus = Some(CpuSet::parse_list(value).context("status: Cpus_allowed_list")?),
            _ => {}
        }
    }
    Ok(Status {
        tgid: tgid.context("status: missing Tgid")?,
        pid: pid.context("status: missing Pid")?,
        uid: uid.context("status: missing Uid")?,
        gid: gid.context("status: missing Gid")?,
        groups: groups.unwrap_or_default(),
        cpus_allowed: cpus.context("status: missing Cpus_allowed_list")?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sched {
    pub policy: Policy,
    /// Kernel-internal priority (`99 - rt_priority` for RT tasks).
    pub prio: i32,
}

/// Parses `/proc/<pid>/sched` (needs `CONFIG_SCHED_DEBUG`).
pub fn parse_sched(s: &str) -> Result<Sched> {
    let (mut policy, mut prio) = (None, None);
    for line in s.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "policy" => policy = Some(Policy::from_raw(value.trim().parse().context("sched: policy")?)),
            "prio" => prio = Some(value.trim().parse().context("sched: prio")?),
            _ => {}
        }
    }
    Ok(Sched { policy: policy.context("sched: missing policy")?, prio: prio.context("sched: missing prio")? })
}

/// A view on `/proc` and `/sys`.
#[derive(Clone, Debug)]
pub struct ProcFs {
    proc_root: PathBuf,
    sys_root: PathBuf,
}

impl ProcFs {
    pub fn new(proc_root: impl Into<PathBuf>, sys_root: impl Into<PathBuf>) -> Self {
        Self { proc_root: proc_root.into(), sys_root: sys_root.into() }
    }

    pub fn system() -> Self {
        Self::new("/proc", "/sys")
    }

    fn cpu_list(&self, name: &str) -> Result<CpuSet> {
        let path = self.sys_root.join("devices/system/cpu").join(name);
        let content = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        CpuSet::parse_list(&content).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn online_cpus(&self) -> Result<CpuSet> {
        self.cpu_list("online")
    }

    pub fn isolated_cpus(&self) -> Result<CpuSet> {
        self.cpu_list("isolated")
    }

    pub fn exe(&self, tgid: i32) -> Option<PathBuf> {
        fs::read_link(self.proc_root.join(tgid.to_string()).join("exe")).ok()
    }

    /// Reads one task. Returns `None` for tasks that are neither RT nor
    /// `SCHED_DEADLINE`, so `status` is only read for the few that matter.
    pub fn read_thread(&self, tgid: i32, tid: i32) -> Result<Option<ThreadInfo>> {
        let dir = self.proc_root.join(tgid.to_string()).join("task").join(tid.to_string());
        let stat = parse_stat(&fs::read_to_string(dir.join("stat"))?)?;
        if !stat.policy.is_rt() && stat.policy != Policy::Deadline {
            return Ok(None);
        }
        let status = parse_status(&fs::read_to_string(dir.join("status"))?)?;
        Ok(Some(ThreadInfo {
            tgid,
            tid,
            comm: stat.comm,
            uid: status.uid,
            gid: status.gid,
            groups: status.groups,
            policy: stat.policy,
            rt_priority: u8::try_from(stat.rt_priority).unwrap_or(u8::MAX),
            kthread: stat.flags & PF_KTHREAD != 0,
            cpus: status.cpus_allowed,
        }))
    }

    /// All RT and `SCHED_DEADLINE` tasks. Tasks that vanish mid-scan are
    /// skipped silently; unparsable ones are logged and skipped.
    pub fn rt_threads(&self) -> Result<Vec<ThreadInfo>> {
        let mut threads = Vec::new();
        for tgid in numeric_entries(&self.proc_root).with_context(|| format!("listing {}", self.proc_root.display()))? {
            let Ok(tids) = numeric_entries(&self.proc_root.join(tgid.to_string()).join("task")) else { continue };
            for tid in tids {
                match self.read_thread(tgid, tid) {
                    Ok(Some(t)) => threads.push(t),
                    Ok(None) => {}
                    Err(e) if is_gone(&e) => {}
                    Err(e) => log_warn!("skipping task {tgid}/{tid}: {e:#}"),
                }
            }
        }
        Ok(threads)
    }
}

fn numeric_entries(dir: &Path) -> io::Result<Vec<i32>> {
    let mut ids: Vec<i32> = fs::read_dir(dir)?.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
    ids.sort_unstable();
    Ok(ids)
}

/// A task that exited between listing and reading.
fn is_gone(e: &anyhow::Error) -> bool {
    e.downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH))
}
