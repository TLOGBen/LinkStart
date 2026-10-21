//! A thin public CLI adapter. All authorization and mutation stays in the daemon.
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};
pub(crate) fn run(a: crate::cli::ToolArgs) -> Result<()> {
    let record: crate::cli::DaemonRecord =
        serde_json::from_slice(&std::fs::read(a.state_dir.join("daemon.json"))?)?;
    let address: SocketAddr = record.address.parse()?;
    if !address.ip().is_loopback() || !crate::daemon::process_alive(record.pid) {
        bail!("discovery does not name a live loopback daemon")
    }
    let credential = std::fs::read_to_string(a.credential_file)?;
    let credential = credential.trim();
    if credential.is_empty() || credential.contains(['\r', '\n']) {
        bail!("invalid credential file")
    }
    let mut body = Vec::new();
    std::io::stdin()
        .take(256 * 1024 + 1)
        .read_to_end(&mut body)?;
    if body.len() > 256 * 1024 {
        bail!("request exceeds 256 KiB")
    }
    let _: linkstart::runtime_core::ToolRequest = serde_json::from_slice(&body)?;
    let (status, value) = post(address, credential, &body)?;
    println!("{}", serde_json::to_string(&value)?);
    if !(200..300).contains(&status) {
        bail!("runtime tool returned HTTP {status}")
    }
    Ok(())
}

/// The caller owns discovery and credentials; this transport never retries an
/// uncertain mutation or prints internal lifecycle responses.
pub(crate) fn post(
    address: SocketAddr,
    credential: &str,
    body: &[u8],
) -> Result<(u16, serde_json::Value)> {
    if !address.ip().is_loopback() {
        bail!("discovery does not name a live loopback daemon")
    }
    if credential.is_empty() || credential.contains(['\r', '\n']) {
        bail!("invalid credential file")
    }
    if body.len() > 256 * 1024 {
        bail!("request exceeds 256 KiB")
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(stream,"POST /v2/tools HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nAuthorization: Bearer {credential}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len())?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut response)?;
    if response.len() > 4 * 1024 * 1024 {
        bail!("response exceeds 4 MiB; outcome may already be committed, inspect using request_id")
    }
    let split = response
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .context("invalid HTTP response; inspect outcome before retry")?;
    let headers = std::str::from_utf8(&response[..split])?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .context("missing HTTP status")?
        .parse::<u16>()?;
    let value: serde_json::Value = serde_json::from_slice(&response[split + 4..])
        .context("invalid JSON response; inspect outcome before retry")?;
    Ok((status, value))
}
