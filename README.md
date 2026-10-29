# p-usher

An usher for realtime threads. p-usher keeps unprivileged `SCHED_FIFO` and
`SCHED_RR` threads off the housekeeping CPUs of a realtime Linux host, and it
tells the thread's owner when it had to step in.

```
cargo install p-usher
```

## Why

On a PREEMPT_RT host with RT throttling disabled, a user thread at any RT
priority that never blocks owns its CPU. If that CPU is a housekeeping CPU, the
thread starves kernel threads (`rcuc/N`, `ksoftirqd/N`, kworkers), and with
them the network, NFS and ssh. The host looks dead and gets reported as broken
hardware. On the isolated CPUs the same thread only competes with its owner's
own work.

## What it does

Every `interval` (default 1 s), p-usher scans `/proc/<pid>/task/<tid>` for
threads with these properties:

- the policy is `SCHED_FIFO` or `SCHED_RR`,
- it is not a kernel thread,
- it is not exempt (root, `exempt_users`, `exempt_groups`, `exempt_comm`).

The **allowed set** A is `/sys/devices/system/cpu/isolated`, or `allowed_cpus`
if set. The **housekeeping set** H is every other online CPU. When a thread's
affinity overlaps H, p-usher does one of two things:

| affinity          | action     | new affinity                               |
|-------------------|------------|--------------------------------------------|
| overlaps A too    | `restrict` | affinity ∩ A                               |
| entirely inside H | `move`     | first CPU of A (or `move_target`)          |

If the kernel refuses (for example because a cgroup cpuset contains no CPU of
A), the result is a `failed` intervention. That thread is then retried with
backoff, starting at 10 s and doubling up to 10 min.

Every intervention is logged to stderr, which goes to the journal under
systemd, and handed to the notifiers:

- **Prometheus textfile** (`[prometheus]`): `<dir>/p_usher.prom` for the
  node_exporter textfile collector. It carries
  `p_usher_interventions_total{user,action,enforced}`,
  `p_usher_last_intervention_timestamp_seconds{user,comm,action,enforced}`
  (these series expire after `retain`), `p_usher_allowed_cpus` and
  `p_usher_up`. It sees every intervention.
- **Email** (`[email]`): a plain-text explanation for the owner. Functional
  accounts go to `admin`. A thread that keeps re-pinning itself onto H is
  enforced every time, but mail goes out at most once per `notify_throttle`
  for each (user, comm, action).

`mode = "report"` makes the same decisions and sends the same notifications
with `enforced = false`, but it never calls `sched_setaffinity`. Use it to roll
p-usher out on a host before it enforces anything.

If A is empty, p-usher evaluates nothing, logs an error and exports
`p_usher_allowed_cpus 0` so you can alert on it.

## What it deliberately does not do

- **It does not cure starvation.** A thread that hogs an isolated CPU stays
  there. Detecting and boosting starved tasks is
  [stalld](https://gitlab.com/rt-linux-tools/stalld)'s job.
- **It does not throttle, kill or demote.** Priorities and policies are left
  alone. Only the affinity changes.
- **It leaves `SCHED_DEADLINE` alone.** Those threads are bound to their root
  domain, so p-usher only logs them.
- **It needs no network to enforce.** The notifiers are best effort: a failing
  relay or a full disk is logged and never delays a scan.

## Pairing with stalld

The two tools cover different CPUs. p-usher keeps H free of user RT threads, so
the kernel's own threads always get to run there. stalld watches for starved
tasks, which on a host running p-usher means the isolated CPUs, and gives those
tasks short `SCHED_DEADLINE` boosts. Run both. Exempt stalld if it runs under a
non-root uid (`exempt_comm = ["^stalld$"]`).

## Running

```
install -m 755 ~/.cargo/bin/p-usher /usr/local/bin/
install -m 644 contrib/p-usher.toml /etc/p-usher.toml
install -m 644 contrib/p-usher.service /etc/systemd/system/
systemctl enable --now p-usher
```

p-usher runs as root, because it needs `CAP_SYS_NICE` to change foreign
threads' affinity. It protects itself with `SCHED_FIFO` priority 95,
`mlockall` and a pin to the first housekeeping CPU. All three are configurable
under `[self]` (`priority = 0`, `mlockall = false`, `pin = false`).

`p-usher --check` validates the configuration. `p-usher --once` runs a single
scan.

## Configuration

See [`contrib/p-usher.toml`](contrib/p-usher.toml). Every key is optional. Keys
that go beyond the example:

```toml
[self]
mlockall = true
pin = true

[email]
starttls = false
username = "p-usher"
password_file = "/etc/p-usher.smtp-password"
functional_users = ["build"]

[email.users]
alice = "alice.smith@example.org"   # wins over the recipient template
```

## Building

The `email` feature (on by default) pulls in lettre with rustls. Use
`--no-default-features` for a build without SMTP.

## License

MIT OR Apache-2.0.
