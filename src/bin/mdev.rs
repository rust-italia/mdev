use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use clap::Parser;
use fork::{daemon, Fork};
use futures_util::StreamExt;
use kobject_uevent::{ActionType, UEvent};
use nix::{
    sys::stat::{makedev, mknod, Mode, SFlag},
    unistd::{chown, unlink},
};
use syslog_tracing::Syslog;
use tokio::{fs, join};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use mdev::{rule, setup_log, RebroadcastMessage, Rebroadcaster};
use mdev_parser::Conf;

#[derive(Parser)]
#[command(after_help = r#"It uses /etc/mdev.conf with lines
[-][ENV=regex;]...DEVNAME UID:GID PERM [>|=PATH]|[!] [@|$|*PROG]

where DEVNAME is device name regex, @major,minor[-minor2], or environment variable regex.

A common use of the latter is to load modules for hotplugged devices:
$MODALIAS=.* 0:0 660 @modprobe "$MODALIAS"

If /dev/mdev.seq file exists, mdev will wait for its value to match $SEQNUM variable. This prevents plug/unplug races.

To activate this feature, create empty /dev/mdev.seq at boot.

If /dev/mdev.log file exists, debug log will be appended to it.
"#)]
struct Opt {
    /// Verbose mode, logs to stderr
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Log to syslog as well
    #[arg(short = 'S', long)]
    syslog: bool,
    /// Scan /sys and populates /dev
    #[arg(short, long)]
    scan: bool,
    /// Daemon mode, listen on netlink
    #[arg(short, long)]
    daemon: bool,
    /// Stay in foreground when in daemon mode
    #[arg(short, long)]
    foreground: bool,
    /// Path to the dev to populate (useful for debugging and testing)
    #[arg(long, default_value = "/dev")]
    devpath: PathBuf,
    /// Rebroadcast events to 0x4 netlink group
    #[arg(long, short)]
    rebroadcast: bool,
}

async fn react_to_event(
    path: &Path,
    env: &HashMap<String, String>,
    action: ActionType,
    conf: &[Conf],
    devpath: &Path,
) -> anyhow::Result<()> {
    let in_sys = Path::new("/sys").join(path.strip_prefix("/")?);
    let dev = fs::read_to_string(&in_sys.join("dev")).await.ok();
    let uevent = fs::read_to_string(&in_sys.join("uevent")).await.ok();

    let devname = if let Some(devname) = env.get("DEVNAME") {
        devname.as_str()
    } else {
        if let Some(ref uevent) = uevent {
            uevent.lines().find_map(|line| {
                if let Some((k, v)) = line.split_once('=') {
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
        // I don't like those unwraps
        .unwrap_or_else(|| path.file_name().unwrap().to_str().unwrap())
    };

    let device_number = if let Some(ref dev) = dev {
        if let Some((maj, min)) = dev.trim().split_once(':') {
            Some((maj.parse::<u32>()?, min.parse::<u32>()?))
        } else {
            None
        }
    } else {
        None
    };

    for rule in conf {
        let devname = if let Some(s) =
            rule::apply(rule, env, device_number, action, devpath, devname).await?
        {
            s
        } else {
            continue;
        };

        let dev_full_path = devpath.join(devname.as_ref());
        let dev_full_dir = dev_full_path.parent().unwrap();

        match action {
            ActionType::Add => {
                if let Some((maj, min)) = device_number {
                    let uid = nix::unistd::User::from_name(&rule.user)?
                        .ok_or_else(|| anyhow!("User {} does not exist", rule.user))?
                        .uid;
                    let gid = nix::unistd::Group::from_name(&rule.group)?
                        .ok_or_else(|| anyhow!("Group {} does not exist", rule.group))?
                        .gid;

                    fs::create_dir_all(dev_full_dir).await?;
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
                    chown(&dev_full_path, Some(uid), Some(gid))?;
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

                    match ev {
                        Ok(ev) => {
                            if let Err(e) =
                                react_to_event(&ev.devpath, &ev.env, ev.action, conf, &self.devpath)
                                    .await
                            {
                                warn!("{e}");
                            }
                            if let Some(rebroadcast_sender) = &rebroadcast_sender {
                                if rebroadcast_sender
                                    .send(RebroadcastMessage::Event(ev))
                                    .await
                                    .is_err()
                                {
                                    warn!("rebroadcaster channel is closed");
                                }
                            }
                        }
                        Err(e) => warn!("{}", e),
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
    #[tokio::main(flavor = "current_thread")]
    async fn run_scan(&self, conf: &[Conf]) -> anyhow::Result<()> {
        let mount_point = Path::new("/sys");
        // WalkDir uses sync fs apis
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

            let ev = UEvent::from_sysfs_path(path, mount_point)?;

            react_to_event(&ev.devpath, &ev.env, ev.action, conf, &self.devpath).await?;
        }

        Ok(())
    }

    fn setup_log(&self) -> anyhow::Result<()> {
        use tracing_subscriber::{fmt, prelude::*};
        if self.daemon && !self.foreground && !self.syslog {
            return Ok(());
        }

        let registry = setup_log(self.verbose);
        let fmt_layer = fmt::layer().with_target(false);

        let mdev_log = Path::new("/dev/mdev.log");
        let file_log = if mdev_log.is_file() {
            let log = std::fs::OpenOptions::new().append(true).open(mdev_log)?;
            let fmt_layer = fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(log);
            Some(fmt_layer)
        } else {
            None
        };

        let registry = registry.with(file_log);

        if self.syslog {
            // SAFETY: They are strings that do not contain a null byte
            let identity = std::env::args()
                .next()
                .map_or_else(|| CString::new("mdev"), |name| CString::new(name))
                .unwrap();
            let syslog = Syslog::new(
                identity,
                syslog_tracing::Options::LOG_PID,
                syslog_tracing::Facility::Daemon,
            )
            .unwrap();
            let fmt_layer = fmt_layer
                .with_level(false)
                .without_time()
                .with_writer(syslog);

            registry.with(fmt_layer).init();
        } else {
            registry.with(fmt_layer).init();
        }

        Ok(())
    }
}

fn run_hotplug(_conf: &[Conf]) -> anyhow::Result<()> {
    unimplemented!()
}

fn main() -> anyhow::Result<()> {
    let conf = if let Ok(input) = std::fs::read_to_string("/etc/mdev.conf") {
        mdev_parser::parse(&input)
    } else {
        vec![Conf::default()]
    };

    if std::env::args().count() == 0 {
        return run_hotplug(&conf);
    }

    let opt = Opt::parse();

    opt.setup_log()?;

    if opt.scan {
        opt.run_scan(&conf)?;
    }

    if opt.daemon {
        if !opt.foreground {
            if let Fork::Child = daemon(false, false).map_err(|_| anyhow::anyhow!("Cannot fork"))? {
                opt.run_daemon(&conf)?;
            }
        } else {
            opt.run_daemon(&conf)?;
        }
    }

    Ok(())
}
