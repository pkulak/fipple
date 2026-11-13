use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, bail};

/// Sends one request line to the daemon and prints its reply. Returns whether
/// the reply was `ok`.
pub fn send(request: &str) -> Result<bool> {
    let path = crate::socket_path()?;
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            bail!("fipple daemon is not running")
        }
        Err(error) => {
            return Err(error).with_context(|| format!("connecting to {}", path.display()));
        }
    };
    writeln!(stream, "{request}")?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    if reply.starts_with("ok") {
        print!("{reply}");
        Ok(true)
    } else {
        eprint!("{reply}");
        Ok(false)
    }
}
