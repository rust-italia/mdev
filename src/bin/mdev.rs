use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fork::{daemon, Fork};
use kobject_uevent::{ActionType, UEvent};
use tracing::{debug, info, warn};

use structopt::clap::AppSettings;
use structopt::StructOpt;

use futures_util::{future, StreamExt};
use walkdir::WalkDir;

use mdev_parser::{Conf, Filter, OnCreation};

#[derive(StructOpt)]
#[structopt(
    setting = AppSettings::ColoredHelp,
    after_help = r#"It uses /etc/mdev.conf with lines
	[-][ENV=regex;]...DEVNAME UID:GID PERM [>|=PATH]|[!] [@|$|*PROG]

where DEVNAME is device name regex, @major,minor[-minor2], or
environment variable regex.

A common use of the latter is to load modules for hotplugged devices:
	$MODALIAS=.* 0:0 660 @modprobe "$MODALIAS"

If /dev/mdev.seq file exists, mdev will wait for its value
to match $SEQNUM variable. This prevents plug/unplug races.

To activate this feature, create empty /dev/mdev.seq at boot.

If /dev/mdev.log file exists, debug log will be appended to it.
"#
)]
struct Opt {
    /// Verbose mode, logs to stderr
    #[structopt(short, long)]
    verbose: bool,
    /// Log to syslog as well
    #[structopt(short = "S", long)]
    syslog: bool,
    /// Scan /sys and populates /dev
    #[structopt(short, long)]
    scan: bool,
    /// Daemon mode, listen on netlink
    #[structopt(short, long)]
    daemon: bool,
    /// Stay in foreground when in daemon mode
    #[structopt(short, long)]
    foreground: bool,
    /// Path to the dev to populate (useful for debugging and testing)
    #[structopt(long, default_value = "/dev", parse(from_os_str))]
    devpath: PathBuf,
}

fn react_to_event(
    path: &Path,
    env: &HashMap<String, String>,
    action: ActionType,
    conf: &[Conf],
    devpath: &Path,
) -> anyhow::Result<()> {
    let dev = std::fs::read_to_string(&path.join("dev"));
    let uevent = std::fs::read_to_string(&path.join("uevent"));

    let devname = if let Some(devname) = env.get("DEVNAME") {
        devname
    } else {
        if let Ok(ref uevent) = uevent {
            uevent.lines().find_map(|line| {
                if let Some((k, v)) = line.split_once("=") {
                    if k == "DEVNAME" {
                        Some(v)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        } else {
            None
        }
        .unwrap_or_else(|| path.file_name().unwrap().to_str().unwrap())
    };

    let device_number = if let Ok(ref dev) = dev {
        if let Some((maj, min)) = dev.split_once(":") {
            Some((maj.parse::<u8>()?, min.parse::<u8>()?))
        } else {
            None
        }
    } else {
        None
    };

    for rule in conf {
        if !rule.envmatches.iter().all(|env_match| {
            env.get(&env_match.envvar)
                .map(|var| env_match.regex.is_match(var))
                .unwrap_or(false)
        }) {
            continue;
        }

        match rule.filter {
            Filter::MajMin(ref device_number_match) => {
                if let Some((maj, min)) = device_number {
                    if maj != device_number_match.maj {
                        continue;
                    }
                    let min2 = device_number_match.min2.unwrap_or(device_number_match.min);
                    if min < device_number_match.min || min > min2 {
                        continue;
                    }
                }
            }
            Filter::DeviceRegex(ref device_regex) => {
                let var = if let Some(ref envvar) = device_regex.envvar {
                    if let Some(var) = env.get(envvar) {
                        var
                    } else {
                        continue;
                    }
                } else {
                    devname
                };
                if !device_regex.regex.is_match(var) {
                    continue;
                }
            }
        }

        info!("rule matched {:?} action {:?}", rule, action);

        if let Some(ref creation) = rule.on_creation {
            match creation {
                OnCreation::Move(to) => {}
                OnCreation::SymLink(to) => {}
                OnCreation::Prevent => {}
            }
        }

        match action {
            ActionType::Add => {}
            ActionType::Remove => {}
            _ => info!("Action {:?}", action),
        }

        // TODO: actual actions

        if rule.stop {
            break;
        }
    }

    Ok(())
}

impl Opt {
    #[tokio::main]
    async fn run_daemon(&self, conf: &[Conf]) -> anyhow::Result<()> {
        info!("mdev daemon starts");
        let fut = mdev::stream::uevents()?.for_each(|ev| {
            info!("event {:?}", ev);
            if let Err(e) = ev
                .and_then(|ev| react_to_event(&ev.devpath, &ev.env, ev.action, conf, &self.devpath))
            {
                warn!("{}", e);
            }
            future::ready(())
        });
        fut.await;

        Ok(())
    }
    fn run_scan(&self, _conf: &[Conf]) -> anyhow::Result<Vec<UEvent>> {
        let mut total_seq = 0;
        let mut uevents = Vec::new();
        for entry in WalkDir::new("/sys/dev") {
            if entry.is_err() {
                continue;
            }
            let e = entry?;
            if let Some(_) = e.path().file_name().filter(|&name| name.eq("dev")) {
                debug!("{}", e.path().display());

                let mut uevent_path = e.path().to_owned();
                uevent_path.pop();
                uevent_path.push("uevent");
                let content = std::fs::read_to_string(&uevent_path);
                if content.is_err() {
                    warn!("content error in {}", uevent_path.display());
                    continue;
                }
                let c = content.unwrap();
                let rows = c.split('\n');
                let mut env = HashMap::new();
                for r in rows {
                    if let Some((key, value)) = r.split_once('=') {
                        let _ = env.insert(key.into(), value.into());
                    }
                }

                let mut ss = e.path().to_owned();
                ss.pop();
                let subsystem = ss.file_stem().unwrap().to_str().unwrap().to_string();
                let mut path = e.path().to_owned();
                path.pop();

                let mut path = e.path().to_owned();
                path.pop();
                uevents.push(UEvent {
                    action: ActionType::Add,
                    devpath: path,
                    subsystem,
                    env,
                    seq: total_seq,
                })
            }
            total_seq += 1;
        }

        Ok(uevents)
    }

    fn setup_log(&self) -> anyhow::Result<()> {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::{fmt, EnvFilter};

        if self.daemon && !self.foreground && !self.syslog {
            return Ok(());
        }

        let fmt_layer = fmt::layer().with_target(false);

        if self.syslog {
            todo!("Wire in syslog somehow");
        }

        let filter_layer = EnvFilter::try_from_default_env()
            .or_else(|_| {
                if self.verbose {
                    EnvFilter::try_new("info")
                } else {
                    EnvFilter::try_new("warn")
                }
            })
            .unwrap();

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(fmt_layer)
            .init();

        Ok(())
    }
}

fn run_hotplug(_conf: &[Conf]) -> anyhow::Result<()> {
    unimplemented!()
}

fn main() -> anyhow::Result<()> {
    let conf = {
        let input = std::fs::read_to_string("/etc/mdev.conf")?;
        mdev_parser::parse(&input)
    };

    if std::env::args().count() == 0 {
        return run_hotplug(&conf);
    }

    let opt = Opt::from_args();

    opt.setup_log()?;

    if opt.scan {
        let uevents = opt.run_scan(&conf)?;
        info!("scan {} number of uevents", uevents.len());
    }

    if opt.daemon {
        if !opt.foreground {
            match daemon(false, false).map_err(|_| anyhow::anyhow!("Cannot fork"))? {
                Fork::Child => opt.run_daemon(&conf)?,
                _ => {}
            }
        } else {
            opt.run_daemon(&conf)?;
        }
    }

    Ok(())
}
