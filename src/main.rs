use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use p_usher::accounts::SystemAccounts;
use p_usher::config::{self, Config, SelfProtection};
use p_usher::engine::{Engine, Env, SystemSetter};
use p_usher::procfs::ProcFs;
use p_usher::throttle::SystemClock;
use p_usher::{log_error, log_info, log_warn, sys};

const USAGE: &str = "\
usage: p-usher [-c|--config PATH] [--once] [--check]

  -c, --config PATH  configuration file (default /etc/p-usher.toml)
      --once         scan once and exit
      --check        validate the configuration and exit
  -V, --version      print the version";

struct Args {
    config: PathBuf,
    explicit: bool,
    once: bool,
    check: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args { config: config::DEFAULT_PATH.into(), explicit: false, once: false, check: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                args.config = it.next().context("--config needs a path")?.into();
                args.explicit = true;
            }
            "--once" => args.once = true,
            "--check" => args.check = true,
            "-V" | "--version" => {
                println!("p-usher {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

fn protect_self(cfg: &SelfProtection, pin: Option<u32>) {
    if cfg.mlockall {
        if let Err(e) = sys::mlockall() {
            log_warn!("mlockall: {e}");
        }
    }
    if cfg.pin {
        match pin {
            Some(cpu) => {
                if let Err(e) = sys::set_affinity(0, &[cpu].into_iter().collect()) {
                    log_warn!("pinning to CPU {cpu}: {e}");
                }
            }
            None => log_warn!("no housekeeping CPU to pin to"),
        }
    }
    if cfg.priority > 0 {
        if let Err(e) = sys::set_fifo(0, cfg.priority) {
            log_warn!("SCHED_FIFO {}: {e}", cfg.priority);
        }
    }
}

fn run() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    let cfg = Config::load(&args.config, args.explicit)?;
    let accounts = SystemAccounts;
    let host = nix::unistd::gethostname()?.to_string_lossy().into_owned();
    let settings = cfg.settings(host, &accounts)?;
    if args.check {
        println!("configuration ok");
        return Ok(());
    }
    let notifiers = cfg.notifiers(&accounts)?;
    let mut engine = Engine::new(
        settings,
        Env {
            procfs: ProcFs::system(),
            setter: Box::new(SystemSetter),
            clock: Arc::new(SystemClock),
            accounts: Box::new(accounts),
            notifiers,
        },
    );
    engine.refresh_sets()?;
    protect_self(&cfg.self_protection, engine.housekeeping().first());

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, Arc::clone(&stop))?;
    }
    log_info!("started (pid {}, interval {})", std::process::id(), humantime::format_duration(cfg.interval));
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if let Err(e) = engine.tick() {
            log_error!("scan: {e:#}");
        }
        if args.once {
            break;
        }
        // Sleep in slices so SIGTERM is honoured promptly with long intervals.
        while !stop.load(Ordering::Relaxed) && started.elapsed() < cfg.interval {
            thread::sleep(cfg.interval.saturating_sub(started.elapsed()).min(Duration::from_millis(200)));
        }
    }
    engine.shutdown();
    log_info!("stopped");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log_error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
