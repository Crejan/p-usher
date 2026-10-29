//! `/etc/p-usher.toml`. Every key is optional.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Deserializer};

use crate::accounts::{Accounts, IdRef};
use crate::cpuset::CpuSet;
use crate::decide::Exemptions;
use crate::engine::{NotifySwitches, Settings};
use crate::model::Mode;
use crate::notify::Notifier;
use crate::notify::prometheus::PrometheusTextfile;

pub const DEFAULT_PATH: &str = "/etc/p-usher.toml";

/// First wait after a failed `sched_setaffinity`; doubles up to `BACKOFF_MAX`.
const BACKOFF_INITIAL: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(600);

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    #[serde(deserialize_with = "duration")]
    pub interval: Duration,
    #[serde(deserialize_with = "cpu_list")]
    pub allowed_cpus: Option<CpuSet>,
    pub move_target: Option<u32>,
    pub exempt_users: Vec<IdRef>,
    pub exempt_groups: Vec<IdRef>,
    pub exempt_comm: Vec<String>,
    #[serde(deserialize_with = "duration")]
    pub notify_throttle: Duration,
    #[serde(rename = "self")]
    pub self_protection: SelfProtection,
    pub notify: NotifySwitches,
    pub prometheus: Option<PrometheusConfig>,
    pub email: Option<EmailConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Enforce,
            interval: Duration::from_secs(1),
            allowed_cpus: None,
            move_target: None,
            exempt_users: Vec::new(),
            exempt_groups: Vec::new(),
            exempt_comm: Vec::new(),
            notify_throttle: Duration::from_secs(3600),
            self_protection: SelfProtection::default(),
            notify: NotifySwitches::default(),
            prometheus: None,
            email: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SelfProtection {
    /// `SCHED_FIFO` priority of the daemon; 0 leaves it at `SCHED_OTHER`.
    pub priority: u8,
    pub mlockall: bool,
    /// Pin the daemon to the first housekeeping CPU.
    pub pin: bool,
}

impl Default for SelfProtection {
    fn default() -> Self {
        Self { priority: 95, mlockall: true, pin: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrometheusConfig {
    pub dir: PathBuf,
    #[serde(deserialize_with = "duration")]
    pub retain: Duration,
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self { dir: "/var/lib/node_exporter".into(), retain: Duration::from_secs(15 * 60) }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmailConfig {
    /// `host[:port]`, port 25 by default.
    pub relay: String,
    pub from: String,
    /// Template, `{user}` is replaced by the user name.
    pub recipient: Option<String>,
    pub users: HashMap<String, String>,
    pub admin: Option<String>,
    pub functional_users: Vec<String>,
    pub functional_groups: Vec<String>,
    pub help_url: Option<String>,
    pub starttls: bool,
    pub username: Option<String>,
    pub password_file: Option<PathBuf>,
}

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

fn cpu_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CpuSet>, D::Error> {
    let s = String::deserialize(d)?;
    CpuSet::parse_list(&s).map(Some).map_err(serde::de::Error::custom)
}

impl Config {
    pub fn parse(s: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Loads `path`. A missing file is fine unless it was named explicitly.
    pub fn load(path: &Path, explicit: bool) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !explicit => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.interval.is_zero() {
            bail!("interval must be positive");
        }
        if let Some(e) = &self.email {
            if e.relay.is_empty() || e.from.is_empty() {
                bail!("[email] needs relay and from");
            }
            if e.recipient.is_none() && e.users.is_empty() && e.admin.is_none() {
                bail!("[email] needs recipient, [email.users] or admin");
            }
        }
        Ok(())
    }

    /// The engine settings; resolves user and group names.
    pub fn settings(&self, host: String, accounts: &dyn Accounts) -> Result<Settings> {
        let mut uids: std::collections::HashSet<u32> = [0].into();
        for u in &self.exempt_users {
            uids.insert(u.uid(accounts).context("exempt_users")?);
        }
        let gids =
            self.exempt_groups.iter().map(|g| g.gid(accounts)).collect::<Result<_>>().context("exempt_groups")?;
        let comm = self
            .exempt_comm
            .iter()
            .map(|r| Regex::new(r).with_context(|| format!("exempt_comm {r:?}")))
            .collect::<Result<_>>()?;
        Ok(Settings {
            host,
            mode: self.mode,
            allowed_override: self.allowed_cpus.clone(),
            move_target: self.move_target,
            exemptions: Exemptions { uids, gids, comm },
            notify: self.notify.clone(),
            notify_throttle: self.notify_throttle,
            backoff_initial: BACKOFF_INITIAL,
            backoff_max: BACKOFF_MAX,
        })
    }

    pub fn notifiers(&self, accounts: &dyn Accounts) -> Result<Vec<Box<dyn Notifier>>> {
        let mut out: Vec<Box<dyn Notifier>> = Vec::new();
        if let Some(p) = &self.prometheus {
            if !p.dir.is_dir() {
                log_warn!("prometheus.dir {} does not exist (yet)", p.dir.display());
            }
            out.push(Box::new(PrometheusTextfile::new(&p.dir, p.retain)));
        }
        if let Some(e) = &self.email {
            out.push(email_notifier(e, accounts)?);
        }
        Ok(out)
    }
}

#[cfg(feature = "email")]
fn email_notifier(cfg: &EmailConfig, accounts: &dyn Accounts) -> Result<Box<dyn Notifier>> {
    use crate::notify::email::{EmailNotifier, Recipients, SmtpMailer};
    let recipients = Recipients::from_config(cfg, accounts)?;
    let mailer = SmtpMailer::new(cfg)?;
    Ok(Box::new(EmailNotifier::new(recipients, cfg.help_url.clone(), Box::new(mailer))?))
}

#[cfg(not(feature = "email"))]
fn email_notifier(_: &EmailConfig, _: &dyn Accounts) -> Result<Box<dyn Notifier>> {
    bail!("[email] is configured but p-usher was built without the `email` feature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::StaticAccounts;

    #[test]
    fn empty_file_gives_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.mode, Mode::Enforce);
        assert_eq!(c.interval, Duration::from_secs(1));
        assert_eq!(c.notify_throttle, Duration::from_secs(3600));
        assert_eq!(c.self_protection.priority, 95);
        assert!(c.notify.restrict && c.notify.r#move && c.notify.failed);
        assert!(c.prometheus.is_none() && c.email.is_none());
    }

    #[test]
    fn shipped_example_parses() {
        let c = Config::parse(include_str!("../contrib/p-usher.toml")).unwrap();
        let p = c.prometheus.unwrap();
        assert_eq!(p.dir, PathBuf::from("/var/lib/node_exporter"));
        assert_eq!(p.retain, Duration::from_secs(900));
    }

    #[test]
    fn full_config() {
        let c = Config::parse(
            r#"
            mode = "report"
            interval = "500ms"
            allowed_cpus = "8-15"
            move_target = 9
            exempt_users = ["alice", 1234]
            exempt_groups = ["rt"]
            exempt_comm = ["^ptp4l$"]
            [notify]
            restrict = false
            [email]
            relay = "mail.example.org:587"
            from = "p-usher@example.org"
            recipient = "{user}@example.org"
            admin = "admin@example.org"
            functional_groups = ["svc"]
            [email.users]
            bob = "robert@example.org"
            "#,
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Report);
        assert_eq!(c.allowed_cpus.as_ref().unwrap().to_string(), "8-15");
        assert!(!c.notify.restrict && c.notify.r#move);
        assert_eq!(c.email.as_ref().unwrap().users["bob"], "robert@example.org");

        let accounts =
            StaticAccounts { users: [(1000, "alice".to_string())].into(), groups: [("rt".to_string(), 77)].into() };
        let s = c.settings("h".into(), &accounts).unwrap();
        assert_eq!(s.exemptions.uids, [0, 1000, 1234].into());
        assert_eq!(s.exemptions.gids, [77].into());
        assert_eq!(s.move_target, Some(9));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "mode = \"off\"",
            "interval = \"soon\"",
            "interval = \"0s\"",
            "allowed_cpus = \"8-\"",
            "unknown = 1",
            "[email]\nrelay = \"x\"",
            "[email]\nrelay = \"x\"\nfrom = \"a@b\"",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        let c = Config::parse("exempt_users = [\"nobody-here\"]").unwrap();
        assert!(c.settings("h".into(), &StaticAccounts::default()).is_err());
        let c = Config::parse("exempt_comm = [\"(\"]").unwrap();
        assert!(c.settings("h".into(), &StaticAccounts::default()).is_err());
    }
}
