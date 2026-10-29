//! Reference adapters without I/O beyond a temp dir: the Prometheus textfile
//! and email rendering / recipient resolution (no SMTP).

mod common;

use std::fs;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use common::cpus;
use p_usher::accounts::StaticAccounts;
use p_usher::config::EmailConfig;
use p_usher::notify::email::{EmailNotifier, Mail, Mailer, Recipients, render};
use p_usher::notify::prometheus::PrometheusTextfile;
use p_usher::{Action, CpuSet, Intervention, Notifier, Policy};

const T0: u64 = 1_800_000_000;

fn ev(user: &str, comm: &str, action: Action, enforced: bool, at: u64) -> Intervention {
    Intervention {
        host: "rt-host".into(),
        at: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
        tgid: 4242,
        tid: 4243,
        comm: comm.into(),
        exe: Some("/opt/lab/bin/control".into()),
        uid: 1000,
        user: user.into(),
        gids: vec![1000],
        policy: Policy::Fifo,
        rt_priority: 80,
        cpus_before: cpus("0-15"),
        cpus_after: if action == Action::Restrict { cpus("8-15") } else { cpus("8") },
        action,
        enforced,
    }
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn textfile_format() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = PrometheusTextfile::new(dir.path(), Duration::from_secs(900));
    p.tick(at(T0), &cpus("8-15")).unwrap();
    p.notify(&ev("alice", "ctl", Action::Restrict, true, T0)).unwrap();
    p.notify(&ev("alice", "ctl", Action::Restrict, true, T0 + 5)).unwrap();
    p.notify(&ev("bob", "a \"b\"\\c", Action::Move, false, T0 + 7)).unwrap();

    let want = format!(
        "\
# HELP p_usher_interventions_total Interventions on RT threads running on housekeeping CPUs.
# TYPE p_usher_interventions_total counter
p_usher_interventions_total{{user=\"alice\",action=\"restrict\",enforced=\"true\"}} 2
p_usher_interventions_total{{user=\"bob\",action=\"move\",enforced=\"false\"}} 1
# HELP p_usher_last_intervention_timestamp_seconds Unix time of the last intervention per user, comm and action.
# TYPE p_usher_last_intervention_timestamp_seconds gauge
p_usher_last_intervention_timestamp_seconds{{user=\"alice\",comm=\"ctl\",action=\"restrict\",enforced=\"true\"}} {}.000
p_usher_last_intervention_timestamp_seconds{{user=\"bob\",comm=\"a \\\"b\\\"\\\\c\",action=\"move\",enforced=\"false\"}} {}.000
# HELP p_usher_allowed_cpus Number of CPUs RT threads are allowed on.
# TYPE p_usher_allowed_cpus gauge
p_usher_allowed_cpus 8
# HELP p_usher_up 1 while p-usher is running.
# TYPE p_usher_up gauge
p_usher_up 1
",
        T0 + 5,
        T0 + 7
    );
    assert_eq!(p.render(), want);
    // Written atomically: the file exists and no temp file is left behind.
    assert_eq!(fs::read_to_string(p.path()).unwrap(), want);
    let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec!["p_usher.prom"]);

    p.shutdown();
    assert!(fs::read_to_string(p.path()).unwrap().ends_with("p_usher_up 0\n"));
}

#[test]
fn textfile_gauges_expire_counters_stay() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = PrometheusTextfile::new(dir.path(), Duration::from_secs(900));
    let allowed = cpus("8-15");
    p.notify(&ev("alice", "old", Action::Move, true, T0)).unwrap();
    p.notify(&ev("alice", "new", Action::Move, true, T0 + 600)).unwrap();

    p.tick(at(T0 + 899), &allowed).unwrap();
    let text = fs::read_to_string(p.path()).unwrap();
    assert!(text.contains("comm=\"old\"") && text.contains("comm=\"new\""));

    p.tick(at(T0 + 900), &allowed).unwrap();
    let text = fs::read_to_string(p.path()).unwrap();
    assert!(!text.contains("comm=\"old\""));
    assert!(text.contains("comm=\"new\""));
    assert!(text.contains("p_usher_interventions_total{user=\"alice\",action=\"move\",enforced=\"true\"} 2"));

    p.tick(at(T0 + 1500), &allowed).unwrap();
    assert!(!fs::read_to_string(p.path()).unwrap().contains("comm="));
}

#[test]
fn textfile_rewritten_when_allowed_set_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = PrometheusTextfile::new(dir.path(), Duration::from_secs(900));
    p.tick(at(T0), &CpuSet::new()).unwrap();
    assert!(fs::read_to_string(p.path()).unwrap().contains("\np_usher_allowed_cpus 0\n"));
    p.tick(at(T0 + 1), &cpus("4-7")).unwrap();
    assert!(fs::read_to_string(p.path()).unwrap().contains("\np_usher_allowed_cpus 4\n"));
}

#[test]
fn email_wording_follows_enforced() {
    let allowed = cpus("8-15");
    let (subject, body) = render(&ev("alice", "ctl", Action::Move, true, T0), &allowed, Some("https://example.org/rt"));
    assert_eq!(subject, "[p-usher] rt-host: realtime thread \"ctl\" (PID 4242) was moved");
    for needle in [
        "Hello alice,",
        "Host:         rt-host",
        "Process:      /opt/lab/bin/control (thread \"ctl\")",
        "PID/TID:      4242/4243",
        "Policy:       SCHED_FIFO, priority 80",
        "CPUs before:  0-15",
        "CPUs after:   8\n",
        "Time:         2027-01-15T08:00:00Z",
        "It was moved to CPU 8.",
        "Block or sleep in the loop",
        "Pin your realtime threads to the allowed CPUs 8-15",
        "taskset -c 8-15",
        "More information: https://example.org/rt",
    ] {
        assert!(body.contains(needle), "missing {needle:?} in\n{body}");
    }

    let (subject, body) = render(&ev("alice", "ctl", Action::Move, false, T0), &allowed, None);
    assert!(subject.ends_with("would have been moved"));
    assert!(body.contains("It would have been moved to CPU 8 (report mode, nothing changed)."));
    assert!(body.contains("CPUs after:   8 (not applied)"));
    assert!(!body.contains("More information"));

    let (subject, body) = render(&ev("alice", "ctl", Action::Restrict, false, T0), &allowed, None);
    assert!(subject.ends_with("would have been restricted"));
    assert!(body.contains("would have been restricted to the allowed CPUs 8-15 (report mode, nothing changed)."));

    let (subject, body) = render(&ev("alice", "ctl", Action::Failed, true, T0), &allowed, None);
    assert!(subject.ends_with("could not be moved"));
    assert!(body.contains("the kernel refused"));
}

fn recipients(toml: &str) -> Recipients {
    let cfg: EmailConfig = toml::from_str(toml).unwrap();
    let accounts = StaticAccounts { groups: [("svc".to_string(), 900)].into(), ..Default::default() };
    Recipients::from_config(&cfg, &accounts).unwrap()
}

#[test]
fn recipient_resolution() {
    let with_template = recipients(
        r#"
        relay = "mail"
        from = "p-usher@example.org"
        recipient = "{user}@example.org"
        admin = "admin@example.org"
        functional_users = ["build"]
        functional_groups = ["svc"]
        [users]
        bob = "robert@lab.example.org"
        "#,
    );
    let map_only = recipients(
        r#"
        relay = "mail"
        from = "p-usher@example.org"
        admin = "admin@example.org"
        [users]
        bob = "robert@lab.example.org"
        "#,
    );
    let no_admin = recipients("relay = \"mail\"\nfrom = \"x@y\"\n[users]\nbob = \"b@y\"");

    let user =
        |name: &str, gids: Vec<u32>| Intervention { user: name.into(), gids, ..ev(name, "c", Action::Move, true, T0) };
    #[rustfmt::skip]
    let table = [
        ("template",                 &with_template, user("alice", vec![1000]),      Some("alice@example.org")),
        ("map entry beats template", &with_template, user("bob", vec![1000]),        Some("robert@lab.example.org")),
        ("functional user",          &with_template, user("build", vec![1000]),      Some("admin@example.org")),
        ("functional group",         &with_template, user("carol", vec![1000, 900]), Some("admin@example.org")),
        ("map hit",                  &map_only,      user("bob", vec![1000]),        Some("robert@lab.example.org")),
        ("no entry, no template",    &map_only,      user("alice", vec![1000]),      Some("admin@example.org")),
        ("nobody to tell",           &no_admin,      user("alice", vec![1000]),      None),
    ];
    for (name, r, ev, want) in table {
        assert_eq!(r.resolve(&ev).as_deref(), want, "{name}");
    }
}

struct ChannelMailer(mpsc::Sender<Mail>);

impl Mailer for ChannelMailer {
    fn send(&self, mail: &Mail) -> anyhow::Result<()> {
        self.0.send(mail.clone())?;
        Ok(())
    }
}

#[test]
fn email_notifier_hands_rendered_mail_to_the_mailer() {
    let (tx, rx) = mpsc::channel();
    let r = Recipients { template: Some("{user}@example.org".into()), ..Default::default() };
    let mut n = EmailNotifier::new(r, None, Box::new(ChannelMailer(tx))).unwrap();
    assert!(n.throttled());
    n.tick(at(T0), &cpus("8-15")).unwrap();
    n.notify(&ev("alice", "ctl", Action::Restrict, true, T0)).unwrap();
    let mail = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(mail.to, "alice@example.org");
    assert!(mail.subject.ends_with("was restricted"));
    assert!(mail.body.contains("taskset -c 8-15"));

    let mut nobody =
        EmailNotifier::new(Recipients::default(), None, Box::new(ChannelMailer(mpsc::channel().0))).unwrap();
    assert!(nobody.notify(&ev("alice", "ctl", Action::Move, true, T0)).is_err());
}
