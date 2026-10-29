//! The few syscalls p-usher needs. Linux only; elsewhere they return
//! `Unsupported` so the crate still builds and its pure parts can be tested.

use std::io;

use crate::cpuset::CpuSet;

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::mem::size_of;

    use libc::c_ulong;

    use crate::cpuset::CpuSet;

    const BITS: usize = c_ulong::BITS as usize;

    fn check(r: libc::c_int) -> io::Result<()> {
        if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    pub fn set_affinity(tid: i32, cpus: &CpuSet) -> io::Result<()> {
        // A mask sized to the highest CPU, so hosts beyond CPU_SETSIZE work too.
        let words = cpus.iter().max().map_or(1, |m| m as usize / BITS + 1);
        let mut mask = vec![0 as c_ulong; words];
        for cpu in cpus.iter() {
            mask[cpu as usize / BITS] |= (1 as c_ulong) << (cpu as usize % BITS);
        }
        check(unsafe { libc::sched_setaffinity(tid, mask.len() * size_of::<c_ulong>(), mask.as_ptr().cast()) })
    }

    pub fn get_affinity(tid: i32) -> io::Result<CpuSet> {
        let mut mask = vec![0 as c_ulong; 8192 / BITS];
        check(unsafe { libc::sched_getaffinity(tid, mask.len() * size_of::<c_ulong>(), mask.as_mut_ptr().cast()) })?;
        Ok((0..mask.len() * BITS)
            .filter(|cpu| mask[cpu / BITS] & ((1 as c_ulong) << (cpu % BITS)) != 0)
            .map(|cpu| cpu as u32)
            .collect())
    }

    fn set_scheduler(tid: i32, policy: libc::c_int, priority: u8) -> io::Result<()> {
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        param.sched_priority = priority.into();
        check(unsafe { libc::sched_setscheduler(tid, policy, &param) })
    }

    pub fn set_fifo(tid: i32, priority: u8) -> io::Result<()> {
        set_scheduler(tid, libc::SCHED_FIFO, priority)
    }

    pub fn set_normal(tid: i32) -> io::Result<()> {
        set_scheduler(tid, libc::SCHED_OTHER, 0)
    }

    pub fn mlockall() -> io::Result<()> {
        check(unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) })
    }

    pub fn gettid() -> i32 {
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::io;

    use crate::cpuset::CpuSet;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn set_affinity(_tid: i32, _cpus: &CpuSet) -> io::Result<()> {
        unsupported()
    }

    pub fn get_affinity(_tid: i32) -> io::Result<CpuSet> {
        unsupported()
    }

    pub fn set_fifo(_tid: i32, _priority: u8) -> io::Result<()> {
        unsupported()
    }

    pub fn set_normal(_tid: i32) -> io::Result<()> {
        unsupported()
    }

    pub fn mlockall() -> io::Result<()> {
        unsupported()
    }

    pub fn gettid() -> i32 {
        0
    }
}

/// `sched_setaffinity(tid, cpus)`.
pub fn set_affinity(tid: i32, cpus: &CpuSet) -> io::Result<()> {
    imp::set_affinity(tid, cpus)
}

/// `sched_getaffinity(tid)`.
pub fn get_affinity(tid: i32) -> io::Result<CpuSet> {
    imp::get_affinity(tid)
}

/// `SCHED_FIFO` at `priority` for `tid` (0 = calling thread).
pub fn set_fifo(tid: i32, priority: u8) -> io::Result<()> {
    imp::set_fifo(tid, priority)
}

/// `SCHED_OTHER` for `tid` (0 = calling thread).
pub fn set_normal(tid: i32) -> io::Result<()> {
    imp::set_normal(tid)
}

pub fn mlockall() -> io::Result<()> {
    imp::mlockall()
}

pub fn gettid() -> i32 {
    imp::gettid()
}
