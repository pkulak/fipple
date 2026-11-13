mod client;
mod daemon;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

const USAGE: &str = "usage: fipple daemon | status | process <dir>";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["daemon"] => daemon::run().map(|()| true),
        ["status"] => client::send("status"),
        ["process", dir] => process(Path::new(dir)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Sends the folder as an absolute path, since the daemon runs elsewhere.
fn process(dir: &Path) -> Result<bool> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{}", dir.display()))?;
    let dir = dir
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", dir.display()))?;
    client::send(&format!("process {dir}"))
}

/// The daemon's Unix socket.
fn socket_path() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(Path::new(&runtime).join("fipple.sock"))
}
