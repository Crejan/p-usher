//! CPU sets in the kernel's list format (`0-3,8,10-11`).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

/// Upper bound for a single CPU number; guards against `0-4294967295`.
const MAX_CPU: u32 = 1 << 16;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CpuSet(BTreeSet<u32>);

impl CpuSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses the list format used by sysfs and `Cpus_allowed_list`.
    pub fn parse_list(s: &str) -> Result<Self> {
        let mut set = BTreeSet::new();
        for part in s.trim().split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let cpu = |v: &str| -> Result<u32> {
                let n: u32 = v.trim().parse().with_context(|| format!("invalid CPU {v:?} in {s:?}"))?;
                if n >= MAX_CPU {
                    bail!("CPU {n} out of range in {s:?}");
                }
                Ok(n)
            };
            let (lo, hi) = match part.split_once('-') {
                Some((a, b)) => (cpu(a)?, cpu(b)?),
                None => (cpu(part)?, cpu(part)?),
            };
            if lo > hi {
                bail!("descending range {part:?} in {s:?}");
            }
            set.extend(lo..=hi);
        }
        Ok(Self(set))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, cpu: u32) -> bool {
        self.0.contains(&cpu)
    }

    /// Lowest CPU of the set.
    pub fn first(&self) -> Option<u32> {
        self.0.first().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().copied()
    }

    pub fn intersection(&self, other: &CpuSet) -> CpuSet {
        self.0.intersection(&other.0).copied().collect()
    }

    pub fn difference(&self, other: &CpuSet) -> CpuSet {
        self.0.difference(&other.0).copied().collect()
    }

    pub fn intersects(&self, other: &CpuSet) -> bool {
        self.0.intersection(&other.0).next().is_some()
    }
}

impl FromIterator<u32> for CpuSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl FromStr for CpuSet {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse_list(s)
    }
}

impl fmt::Display for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut it = self.0.iter().copied().peekable();
        let mut sep = "";
        while let Some(start) = it.next() {
            let mut end = start;
            while it.peek() == Some(&(end + 1)) {
                end += 1;
                it.next();
            }
            if start == end {
                write!(f, "{sep}{start}")?;
            } else {
                write!(f, "{sep}{start}-{end}")?;
            }
            sep = ",";
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_lists() {
        for (input, cpus, shown) in [
            ("", vec![], ""),
            ("\n", vec![], ""),
            ("3", vec![3], "3"),
            ("0-3", vec![0, 1, 2, 3], "0-3"),
            ("0-1,4,6-7\n", vec![0, 1, 4, 6, 7], "0-1,4,6-7"),
            ("5,1,2", vec![1, 2, 5], "1-2,5"),
        ] {
            let set = CpuSet::parse_list(input).unwrap();
            assert_eq!(set, cpus.iter().copied().collect(), "{input:?}");
            assert_eq!(set.to_string(), shown, "{input:?}");
        }
    }

    #[test]
    fn rejects_garbage() {
        for input in ["a", "3-1", "1-", "99999999", "0-70000"] {
            assert!(CpuSet::parse_list(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn set_operations() {
        let a: CpuSet = "0-3".parse().unwrap();
        let b: CpuSet = "2-5".parse().unwrap();
        assert_eq!(a.intersection(&b).to_string(), "2-3");
        assert_eq!(a.difference(&b).to_string(), "0-1");
        assert!(a.intersects(&b));
        assert!(!a.intersects(&"8".parse().unwrap()));
        assert_eq!(b.first(), Some(2));
    }
}
