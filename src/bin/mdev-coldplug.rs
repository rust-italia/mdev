use std::path::PathBuf;

use clap::Parser;
use mdev::setup_log;
use tracing::{debug, error};
use walkdir::WalkDir;

#[derive(Parser)]
struct Opt {
    /// Verbose mode, logs to stderr
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Path where the sysfs is mounted
    #[arg(short, long, default_value = "/sys")]
    sysfs_mount: PathBuf,
}

impl Opt {
    fn setup_log(&self) {
        use tracing_subscriber::{fmt, prelude::*};
        let fmt_layer = fmt::layer().with_target(false);
        setup_log(self.verbose).with(fmt_layer).init();
    }
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    opt.setup_log();

    let classdir = WalkDir::new(opt.sysfs_mount.join("class"))
        .follow_links(true)
        .max_depth(4)
        .into_iter();
    let busdir = WalkDir::new(opt.sysfs_mount.join("bus"))
        .follow_links(true)
        .max_depth(3)
        .into_iter();

    for entry in classdir
        .chain(busdir)
        .filter_map(|e| e.ok().filter(|e| e.file_name().eq("uevent")))
    {
        debug!("{entry:?}");
        let p = entry.path();
        std::fs::write(p, "add").unwrap_or_else(|e| error!("cannot write to {}: {e}", p.display()));
    }

    Ok(())
}
