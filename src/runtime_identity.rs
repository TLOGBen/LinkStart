//! Incarnation correlation only; successful inspection does not authorize tools.
use crate::{
    cli::DaemonRecord,
    util::{PROTOCOL, VERSION},
};
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

pub(crate) enum Inspection {
    Reusable(SocketAddr),
    Stale,
}

pub(crate) fn inspect(record: &DaemonRecord) -> Result<Inspection> {
    let address: SocketAddr = record
        .address
        .parse()
        .context("runtime_identity_unknown: invalid discovery address")?;
    if !address.ip().is_loopback() || address.port() == 0 {
        bail!("runtime_identity_conflict: discovery must name a loopback listener")
    }
    let alive = crate::daemon::process_alive(record.pid);
    // Windows may deliver loopback refusal after its first TCP retry (~2s).
    // A shorter deadline turns a closed listener into Unknown, never Stale.
    let deadline = Instant::now() + Duration::from_millis(2600);
    let mut stream = match TcpStream::connect_timeout(&address, Duration::from_millis(2200)) {
        Ok(stream) => stream,
        Err(error) if !alive && error.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Ok(Inspection::Stale)
        }
        Err(_) => bail!("runtime_identity_unknown: listener identity unavailable"),
    };
    if !alive {
        bail!("runtime_identity_conflict: dead recorded PID has an accepting listener")
    }
    let instance = record.instance_id.as_deref().filter(|id| !id.is_empty())
        .context("runtime_identity_unknown: legacy discovery missing instanceId; stop using compatible CLI before upgrading")?;
    if record.version != VERSION || record.protocol_major != PROTOCOL {
        bail!("runtime_identity_conflict: daemon_version_or_protocol_conflict: explicit drain/restart required")
    }
    stream.set_write_timeout(Some(Duration::from_millis(150)))?;
    write!(
        stream,
        "GET /v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .context("runtime_identity_unknown: health request failed")?;
    let mut response = Vec::new();
    let mut buffer = [0; 2048];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("runtime_identity_unknown: health deadline exceeded")?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream
            .read(&mut buffer)
            .context("runtime_identity_unknown: health read failed")?;
        if count == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..count]);
        if response.len() > 16 * 1024 {
            bail!("runtime_identity_unknown: health response too large")
        }
    }
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .context("runtime_identity_unknown: malformed health HTTP")?;
    let headers = std::str::from_utf8(&response[..split])
        .context("runtime_identity_unknown: malformed headers")?;
    let line = headers.lines().next().unwrap_or("");
    if !line.starts_with("HTTP/1.1 200 ") && !line.starts_with("HTTP/1.0 200 ") {
        bail!("runtime_identity_unknown: health HTTP status is not 200")
    }
    let health: serde_json::Value = serde_json::from_slice(&response[split + 4..])
        .context("runtime_identity_unknown: malformed health JSON")?;
    if health["status"] != "ready"
        || health["pid"].as_u64() != Some(record.pid as u64)
        || health["address"].as_str() != Some(record.address.as_str())
        || health["version"].as_str() != Some(record.version.as_str())
        || health["protocolMajor"].as_str() != Some(record.protocol_major.as_str())
        || health["instanceId"].as_str() != Some(instance)
    {
        bail!("runtime_identity_conflict: health does not match recorded Runtime incarnation")
    }
    Ok(Inspection::Reusable(address))
}

pub(crate) fn require(record: &DaemonRecord) -> Result<SocketAddr> {
    match inspect(record)? {
        Inspection::Reusable(address) => Ok(address),
        Inspection::Stale => bail!("runtime_identity_unknown: recorded Runtime has exited"),
    }
}
