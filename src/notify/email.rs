//! Reference adapter: a plain-text mail to the thread's owner (or the admin).
//!
//! Rendering and recipient resolution are pure; sending happens on a worker
//! thread behind [`Mailer`] so a slow relay never stalls a scan.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;

use anyhow::{Result, anyhow};

use super::Notifier;
use crate::accounts::{Accounts, IdRef};
use crate::config::EmailConfig;
use crate::cpuset::CpuSet;
use crate::model::{Action, Intervention};

/// Mails waiting for the relay; beyond this they are dropped (and logged).
const QUEUE: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct Recipients {
    pub template: Option<String>,
    pub users: HashMap<String, String>,
    pub admin: Option<String>,
    pub functional_users: HashSet<String>,
    pub functional_gids: HashSet<u32>,
}

impl Recipients {
    pub fn from_config(cfg: &EmailConfig, accounts: &dyn Accounts) -> Result<Self> {
        Ok(Self {
            template: cfg.recipient.clone(),
            users: cfg.users.clone(),
            admin: cfg.admin.clone(),
            functional_users: cfg.functional_users.iter().cloned().collect(),
            functional_gids: cfg
                .functional_groups
                .iter()
                .map(|g| IdRef::Name(g.clone()).gid(accounts))
                .collect::<Result<_>>()?,
        })
    }

    /// Functional accounts go to the admin; otherwise the `[email.users]`
    /// entry, then the template, then the admin.
    pub fn resolve(&self, ev: &Intervention) -> Option<String> {
        let functional = self.functional_users.contains(&ev.user)
            || self.functional_users.contains(&ev.uid.to_string())
            || ev.gids.iter().any(|g| self.functional_gids.contains(g));
        if functional {
            return self.admin.clone();
        }
        if let Some(addr) = self.users.get(&ev.user) {
            return Some(addr.clone());
        }
        match &self.template {
            Some(t) => Some(t.replace("{user}", &ev.user)),
            None => self.admin.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Subject and body for one intervention. `allowed` is the current A.
pub fn render(ev: &Intervention, allowed: &CpuSet, help_url: Option<&str>) -> (String, String) {
    let process = match &ev.exe {
        Some(exe) => format!("{} (thread \"{}\")", exe.display(), ev.comm),
        None => format!("thread \"{}\"", ev.comm),
    };
    let report = " (report mode, nothing changed)";
    let (verb, what) = match (ev.action, ev.enforced) {
        (Action::Restrict, true) => {
            ("was restricted", format!("Its CPU affinity was restricted to the allowed CPUs {}.", ev.cpus_after))
        }
        (Action::Restrict, false) => (
            "would have been restricted",
            format!("Its CPU affinity would have been restricted to the allowed CPUs {}{report}.", ev.cpus_after),
        ),
        (Action::Move, true) => ("was moved", format!("It was moved to CPU {}.", ev.cpus_after)),
        (Action::Move, false) => {
            ("would have been moved", format!("It would have been moved to CPU {}{report}.", ev.cpus_after))
        }
        (Action::Failed, _) => (
            "could not be moved",
            format!(
                "p-usher tried to set its affinity to CPU {} but the kernel refused (for example a cgroup \
                 cpuset without any allowed CPU). It still runs on housekeeping CPUs.",
                ev.cpus_after
            ),
        ),
    };
    let subject = format!("[p-usher] {}: realtime thread \"{}\" (PID {}) {verb}", ev.host, ev.comm, ev.tgid);

    let mut b = String::new();
    let _ = writeln!(b, "Hello {},\n", ev.user);
    let _ = writeln!(b, "p-usher on {} found a realtime thread of yours on the housekeeping CPUs.\n", ev.host);
    let _ = writeln!(b, "  Host:         {}", ev.host);
    let _ = writeln!(b, "  Process:      {process}");
    let _ = writeln!(b, "  PID/TID:      {}/{}", ev.tgid, ev.tid);
    let _ = writeln!(b, "  Policy:       {}, priority {}", ev.policy, ev.rt_priority);
    let _ = writeln!(b, "  CPUs before:  {}", ev.cpus_before);
    let _ = writeln!(b, "  CPUs after:   {}{}", ev.cpus_after, if ev.enforced { "" } else { " (not applied)" });
    let _ = writeln!(b, "  Time:         {}\n", humantime::format_rfc3339_seconds(ev.at));
    let _ = writeln!(b, "{what}\n");
    let _ = writeln!(
        b,
        "What this means: a SCHED_FIFO/SCHED_RR thread that never blocks owns its CPU. On a\n\
         housekeeping CPU it starves the kernel threads that run the network, storage and ssh,\n\
         and the whole host looks dead. On the allowed (isolated) CPUs {allowed} it only\n\
         competes with your own work.\n"
    );
    let _ = writeln!(
        b,
        "What to do:\n  \
         - Block or sleep in the loop (wait on a timer, socket or condition variable instead\n    \
           of busy-polling).\n  \
         - Pin your realtime threads to the allowed CPUs {allowed} yourself, e.g. with\n    \
           pthread_setaffinity_np(3) or by starting the program with `taskset -c {allowed}`.\n"
    );
    if let Some(url) = help_url {
        let _ = writeln!(b, "More information: {url}\n");
    }
    let _ = write!(b, "-- \np-usher on {}\n", ev.host);
    (subject, b)
}

pub trait Mailer: Send {
    fn send(&self, mail: &Mail) -> Result<()>;
}

pub struct EmailNotifier {
    recipients: Recipients,
    help_url: Option<String>,
    allowed: CpuSet,
    queue: SyncSender<Mail>,
}

impl EmailNotifier {
    /// Starts the worker thread that hands mails to `mailer`.
    pub fn new(recipients: Recipients, help_url: Option<String>, mailer: Box<dyn Mailer>) -> Result<Self> {
        let (queue, rx) = mpsc::sync_channel::<Mail>(QUEUE);
        thread::Builder::new().name("p-usher-mail".into()).spawn(move || {
            // Do not inherit the daemon's SCHED_FIFO for network I/O.
            let _ = crate::sys::set_normal(0);
            for mail in rx {
                if let Err(e) = mailer.send(&mail) {
                    log_warn!("email to {}: {e:#}", mail.to);
                }
            }
        })?;
        Ok(Self { recipients, help_url, allowed: CpuSet::new(), queue })
    }
}

impl Notifier for EmailNotifier {
    fn notify(&mut self, ev: &Intervention) -> Result<()> {
        let to = self
            .recipients
            .resolve(ev)
            .ok_or_else(|| anyhow!("no recipient for user {} (set recipient, [email.users] or admin)", ev.user))?;
        let (subject, body) = render(ev, &self.allowed, self.help_url.as_deref());
        match self.queue.try_send(Mail { to, subject, body }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(m)) => Err(anyhow!("mail queue full, dropped mail to {}", m.to)),
            Err(TrySendError::Disconnected(_)) => Err(anyhow!("mail worker is gone")),
        }
    }

    fn tick(&mut self, _now: std::time::SystemTime, allowed: &CpuSet) -> Result<()> {
        self.allowed = allowed.clone();
        Ok(())
    }
}

#[cfg(feature = "email")]
pub use smtp::SmtpMailer;

#[cfg(feature = "email")]
mod smtp {
    use std::fs;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use lettre::message::Mailbox;
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    use super::{Mail, Mailer};
    use crate::config::EmailConfig;

    pub struct SmtpMailer {
        transport: SmtpTransport,
        from: Mailbox,
    }

    impl SmtpMailer {
        pub fn new(cfg: &EmailConfig) -> Result<Self> {
            let (host, port) = match cfg.relay.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().with_context(|| format!("email.relay: bad port in {:?}", cfg.relay))?),
                None => (cfg.relay.as_str(), 25),
            };
            let mut builder = if cfg.starttls {
                SmtpTransport::starttls_relay(host).context("email.relay")?
            } else {
                SmtpTransport::builder_dangerous(host)
            };
            builder = builder.port(port).timeout(Some(Duration::from_secs(30)));
            if let Some(user) = &cfg.username {
                let file = cfg.password_file.as_ref().context("email.username needs email.password_file")?;
                let password = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
                builder = builder.credentials(Credentials::new(user.clone(), password.trim_end().to_string()));
            }
            Ok(Self { transport: builder.build(), from: cfg.from.parse().context("email.from")? })
        }
    }

    impl Mailer for SmtpMailer {
        fn send(&self, mail: &Mail) -> Result<()> {
            let msg = Message::builder()
                .from(self.from.clone())
                .to(mail.to.parse().with_context(|| format!("recipient {:?}", mail.to))?)
                .subject(&mail.subject)
                .header(ContentType::TEXT_PLAIN)
                .body(mail.body.clone())?;
            self.transport.send(&msg)?;
            Ok(())
        }
    }
}
