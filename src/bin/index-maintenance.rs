#![cfg_attr(windows, windows_subsystem = "windows")]
use anyhow::{Context, Result, bail};
use llmlocalsearch::settings;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--delete-current-index")) {
        bail!("invalid operation");
    }
    let install_dir = PathBuf::from(args.next().context("missing installation directory")?);
    let data_dir = if let Some(value) = args.next() {
        if value != "--settings-dir" {
            bail!("invalid argument");
        }
        PathBuf::from(args.next().context("missing settings directory")?)
    } else {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("missing local app data")?)
    };
    let config = settings::load(
        &data_dir.join("LLMLocalSearch").join("settings.json"),
        &install_dir,
    )?;
    settings::remove_index(&config.index_dir)
}
