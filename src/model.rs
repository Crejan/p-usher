//! The data p-usher reasons about.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;

use crate::cpuset::CpuSet;

/// Scheduling policy as reported in field 41 of `/proc/<tid>/stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Policy {
    Other,
    Fifo,
    Rr,
    Batch,
    Idle,
    Deadline,
    Unknown(u32),
}

impl Policy {
    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => Self::Other,
            1 => Self::Fifo,
            2 => Self::Rr,
            3 => Self::Batch,
            5 => Self::Idle,
            6 => Self::Deadline,
            v => Self::Unknown(v),
        }
    }

    /// `SCHED_FIFO` or `SCHED_RR`: the policies p-usher enforces.
    pub fn is_rt(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other => f.write_str("SCHED_OTHER"),
            Self::Fifo => f.write_str("SCHED_FIFO"),
            Self::Rr => f.write_str("SCHED_RR"),
            Self::Batch => f.write_str("SCHED_BATCH"),
            Self::Idle => f.write_str("SCHED_IDLE"),
            Self::Deadline => f.write_str("SCHED_DEADLINE"),
            Self::Unknown(v) => write!(f, "policy {v}"),
        }
    }
}

/// What p-usher decided for a violating thread. The decision is the same in
/// both modes; `Intervention::enforced` tells whether it was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Restrict,
    Move,
    Failed,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restrict => "restrict",
            Self::Move => "move",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Enforce,
    Report,
}

/// One task as read from `/proc/<tgid>/task/<tid>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadInfo {
    pub tgid: i32,
    pub tid: i32,
    pub comm: String,
    /// Real uid: the person who started the work.
    pub uid: u32,
    /// Real primary gid.
    pub gid: u32,
    /// Supplementary groups (`Groups:`).
    pub groups: Vec<u32>,
    pub policy: Policy,
    pub rt_priority: u8,
    pub kthread: bool,
    pub cpus: CpuSet,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Intervention {
    pub host: String,
    pub at: SystemTime,
    pub tgid: i32,
    pub tid: i32,
    pub comm: String,
    pub exe: Option<PathBuf>,
    pub uid: u32,
    pub user: String,
    /// Primary gid plus supplementary groups; lets notifiers route on groups.
    pub gids: Vec<u32>,
    pub policy: Policy,
    pub rt_priority: u8,
    pub cpus_before: CpuSet,
    /// The target affinity; applied iff `enforced` and `action != Failed`.
    pub cpus_after: CpuSet,
    pub action: Action,
    /// False in report mode; `Failed` implies true.
    pub enforced: bool,
}
