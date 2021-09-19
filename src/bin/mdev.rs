use tracing::{info, warn};

use structopt::clap::AppSettings;
use structopt::StructOpt;

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
}

impl Opt {
    fn run_daemon(&self) -> anyhow::Result<()> {
        info!("mdev daemon starts");
        unimplemented!()
    }
    fn run_scan(&self) -> anyhow::Result<()> {
        info!("Scanning /sys and populating /dev");
        unimplemented!()
    }

    fn setup_log(&self) -> anyhow::Result<()> {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::{fmt, EnvFilter};

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

fn run_hotplug() -> anyhow::Result<()> {
    unimplemented!()
}

fn main() -> anyhow::Result<()> {
    if std::env::args().count() == 0 {
        return run_hotplug();
    }

    let opt = Opt::from_args();

    opt.setup_log()?;

    if opt.scan {
        opt.run_scan()?;
    }

    if opt.daemon {
        opt.run_daemon()?;
    }

    Ok(())
}
