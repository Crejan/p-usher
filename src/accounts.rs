//! User and group name lookups, behind a trait so tests need no NSS.

use std::collections::HashMap;

use anyhow::{Context, Result};
use nix::unistd::{Group, Uid, User};
use serde::Deserialize;

pub trait Accounts: Send {
    fn user_name(&self, uid: u32) -> Option<String>;
    fn uid_by_name(&self, name: &str) -> Option<u32>;
    fn gid_by_name(&self, name: &str) -> Option<u32>;
}

/// Lookups through libc/NSS (`/etc/passwd`, LDAP, ...).
pub struct SystemAccounts;

impl Accounts for SystemAccounts {
    fn user_name(&self, uid: u32) -> Option<String> {
        User::from_uid(Uid::from_raw(uid)).ok().flatten().map(|u| u.name)
    }

    fn uid_by_name(&self, name: &str) -> Option<u32> {
        User::from_name(name).ok().flatten().map(|u| u.uid.as_raw())
    }

    fn gid_by_name(&self, name: &str) -> Option<u32> {
        Group::from_name(name).ok().flatten().map(|g| g.gid.as_raw())
    }
}

/// Fixed tables, for tests.
#[derive(Clone, Default)]
pub struct StaticAccounts {
    pub users: HashMap<u32, String>,
    pub groups: HashMap<String, u32>,
}

impl Accounts for StaticAccounts {
    fn user_name(&self, uid: u32) -> Option<String> {
        self.users.get(&uid).cloned()
    }

    fn uid_by_name(&self, name: &str) -> Option<u32> {
        self.users.iter().find(|(_, n)| *n == name).map(|(uid, _)| *uid)
    }

    fn gid_by_name(&self, name: &str) -> Option<u32> {
        self.groups.get(name).copied()
    }
}

/// A user or group in the config: a name, or a numeric id (as a TOML integer
/// or a string of digits).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IdRef {
    Id(u32),
    Name(String),
}

impl IdRef {
    fn resolve(&self, kind: &str, lookup: impl Fn(&str) -> Option<u32>) -> Result<u32> {
        match self {
            Self::Id(id) => Ok(*id),
            Self::Name(n) => match n.parse() {
                Ok(id) => Ok(id),
                Err(_) => lookup(n).with_context(|| format!("unknown {kind} {n:?}")),
            },
        }
    }

    pub fn uid(&self, accounts: &dyn Accounts) -> Result<u32> {
        self.resolve("user", |n| accounts.uid_by_name(n))
    }

    pub fn gid(&self, accounts: &dyn Accounts) -> Result<u32> {
        self.resolve("group", |n| accounts.gid_by_name(n))
    }
}
