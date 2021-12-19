use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{create_dir_all, rename};
use std::os::unix::fs::symlink;
use std::os::unix::prelude::OsStrExt;
use std::path::{Path, PathBuf, Component};

use fork::{daemon, Fork};
use kobject_uevent::{ActionType, UEvent};
use nix::sys::stat::{makedev, mknod, Mode, SFlag};
use nix::unistd::unlink;
use tokio::join;
use tracing::{debug, info, warn};

use structopt::clap::AppSettings;
use structopt::StructOpt;

use futures_util::StreamExt;
use walkdir::WalkDir;

use mdev::{RebroadcastMessage, Rebroadcaster};
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
    #[structopt(short, long, parse(from_occurrences))]
    verbose: u8,
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
    /// Rebroadcast events to 0x4 netlink group
    #[structopt(long, short)]
    rebroadcast: bool,
}

fn react_to_event(
    path: &Path,
    env: &HashMap<String, String>,
    action: ActionType,
    conf: &[Conf],
    devpath: &Path,
) -> anyhow::Result<()> {
    let in_sys = Path::new("/sys").join(path.strip_prefix("/")?);
    let dev = std::fs::read_to_string(&in_sys.join("dev"));
    let uevent = std::fs::read_to_string(&in_sys.join("uevent"));

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
        if let Some((maj, min)) = dev.trim().split_once(":") {
            Some((maj.parse::<u32>()?, min.parse::<u32>()?))
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

        // to avoid unneeded allocations
        let mut on_creation: Option<Cow<OnCreation>> = rule.on_creation.as_ref().map(|t| Cow::Borrowed(t));

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
                if let Some(old_on_creation) = on_creation {
                    // this creates a collection of OsString:Ostring
                    // because is lighter and quicker having matches already indexed
                    // than converting to usize every OsStr that starts by %
                    // the counterpart is that we allocate every match instead of keeping the reference to the original str
                    let matches: HashMap<OsString, OsString> = device_regex.regex.find_iter(var)
                        .enumerate()
                        .map(|(index, m)| {
                            (format!("%{}", index).into(), m.as_str().into())
                        })
                        .collect();
                    if matches.is_empty() {
                        continue;
                    }

                    let mut new_on_creation = old_on_creation.into_owned();
                    match &mut new_on_creation {
                        OnCreation::Move(pb) => {
                            *pb = replace_in_pathbuf(pb, &matches);
                        },
                        OnCreation::SymLink(pb) => {
                            *pb = replace_in_pathbuf(pb, &matches);
                        },
                        _ => {},
                    }
                    on_creation = Some(Cow::Owned(new_on_creation));
                }
                else {
                    if !device_regex.regex.is_match(var) {
                        continue;
                    }
                }
            }
        }

        info!("rule matched {:?} action {:?}", rule, action);

        // WARNING: WIP code
        if let Some(creation) = on_creation.as_deref() {
            match creation {
                OnCreation::Move(to) => {
                    debug!("Rename {} to {}", devname, to.display());
                    let (dir, target) = if is_dir(to) {
                        (to.as_path(), to.join(devname))
                    }
                    else {
                        // not sure about using "" as fallback
                        (to.parent().unwrap_or_else(|| Path::new("")), to.clone())
                    };
                    create_dir_all(devpath.join(dir))?;
                    rename(devpath.join(devname), devpath.join(target))?;
                }
                OnCreation::SymLink(to) => {
                    debug!("Link {} to {}", devname, to.display());
                    let (dir, target) = if is_dir(to) {
                        (to.as_path(), to.join(devname))
                    }
                    else {
                        // not sure about using "" as fallback
                        (to.parent().unwrap_or_else(|| Path::new("")), to.clone())
                    };
                    create_dir_all(devpath.join(dir))?;
                    symlink(devpath.join(devname), devpath.join(target))?;
                }
                OnCreation::Prevent => {
                    debug!("Do not create node");
                    continue;
                }
            }
        }

        let dev_full_path = devpath.join(devname);
        let dev_full_dir = dev_full_path.parent().unwrap();

        match action {
            ActionType::Add => {
                if let Some((maj, min)) = device_number {
                    create_dir_all(dev_full_dir)?;
                    let kind = if path.iter().any(|v| v == OsStr::new("block")) {
                        SFlag::S_IFBLK
                    } else {
                        SFlag::S_IFCHR
                    };
                    let mode = Mode::from_bits(rule.mode)
                        .ok_or_else(|| anyhow::anyhow!("Invalid mode"))?;
                    let dev = makedev(maj.into(), min.into());

                    info!(
                        "Creating {:?} {:?} {:?} {:?}",
                        dev_full_path, kind, mode, dev
                    );
                    mknod(&dev_full_path, kind, mode, dev)?;
                }
            }
            ActionType::Remove => {
                info!("Removing {:?}", dev_full_path);
                unlink(&dev_full_path)?;
            }
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

        // Waiting for `Option::unzip` or try_blocks
        let (rebroadcaster, rebroadcast_sender) = match self
            .rebroadcast
            .then(|| Rebroadcaster::new(16))
            .transpose()?
        {
            Some((rebroadcaster, sender)) => (Some(rebroadcaster), Some(sender)),
            None => (None, None),
        };

        let reactor_fut = async {
            mdev::stream::uevents()?
                .for_each(|ev| async {
                    info!("event {:?}", ev);
                    let result = ev.map(|ev| async {
                        let result =
                            react_to_event(&ev.devpath, &ev.env, ev.action, conf, &self.devpath);
                        if let Some(rebroadcast_sender) = &rebroadcast_sender {
                            if rebroadcast_sender
                                .send(RebroadcastMessage::Event(ev))
                                .await
                                .is_err()
                            {
                                panic!("rebroadcaster channel is closed");
                            }
                        }
                        result
                    });
                    if let Err(e) = result {
                        warn!("{}", e);
                    }
                })
                .await;

            if let Some(rebroadcast_sender) = &rebroadcast_sender {
                if rebroadcast_sender
                    .send(RebroadcastMessage::Stop)
                    .await
                    .is_err()
                {
                    panic!("rebroadcaster channel is closed");
                }
            }
            Ok(())
        };

        match rebroadcaster {
            Some(rebroadcaster) => join!(reactor_fut, rebroadcaster).0,
            None => reactor_fut.await,
        }
    }
    fn run_scan(&self, conf: &[Conf]) -> anyhow::Result<()> {
        let mount_point = Path::new("/sys");
        let walk = WalkDir::new(mount_point.join("dev"))
            .follow_links(true)
            .max_depth(3)
            .into_iter();

        for e in walk.filter_map(|p| {
            if let Ok(p) = p {
                if p.file_name() == "dev" && p.depth() != 0 {
                    Some(p)
                } else {
                    None
                }
            } else {
                None
            }
        }) {
            let path = e
                .path()
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Scanning an impossible path {:?}", e.path()))?;
            debug!("{:?}", path);

            let ev = UEvent::from_sysfs_path(path, &mount_point)?;

            react_to_event(&ev.devpath, &ev.env, ev.action, conf, &self.devpath)?;
        }

        Ok(())
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
                if self.verbose < 1 {
                    EnvFilter::try_new("info")
                } else if self.verbose < 2 {
                    EnvFilter::try_new("warn")
                } else {
                    EnvFilter::try_new("debug")
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

fn is_dir(path: &PathBuf) -> bool {
    path.as_os_str()
        .as_bytes()
        .ends_with(&[b'/'])
}

fn replace_in_pathbuf(pb: &PathBuf, matches: &HashMap<OsString, OsString>) -> PathBuf {
    pb.components()
        .map(|c| {
            if let Component::Normal(s) = c {
                if let Some(m) = matches.get(s) {
                    return Component::Normal(&m);
                }
            }
            c
        })
        .collect()
}

fn run_hotplug(_conf: &[Conf]) -> anyhow::Result<()> {
    unimplemented!()
}

fn main() -> anyhow::Result<()> {
    let conf = if let Ok(input) = std::fs::read_to_string("/etc/mdev.conf") {
        mdev_parser::parse(&input)
    } else {
        Default::default()
    };

    if std::env::args().count() == 0 {
        return run_hotplug(&conf);
    }

    let opt = Opt::from_args();

    opt.setup_log()?;

    if opt.scan {
        opt.run_scan(&conf)?;
    }

    if opt.daemon {
        if !opt.foreground {
            if let Fork::Child = daemon(false, false).map_err(|_| anyhow::anyhow!("Cannot fork"))? {
                opt.run_daemon(&conf)?
            }
        } else {
            opt.run_daemon(&conf)?;
        }
    }

    Ok(())
}
