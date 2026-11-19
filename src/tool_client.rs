//! A thin public CLI adapter. All authorization and mutation stays in the daemon.
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};
pub(crate) fn run(a: crate::cli::ToolArgs) -> Result<()> {
    let bytes = crate::discovery::read(&a.state_dir)?.context("daemon discovery record missing")?;
    let record: crate::cli::DaemonRecord = serde_json::from_slice(&bytes)?;
    let mut peer = crate::runtime_identity::require(&record)?;
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
    let (status, value) = post(&mut peer, credential, &body)?;
    println!("{}", serde_json::to_string(&value)?);
    if !(200..300).contains(&status) {
        bail!("runtime tool returned HTTP {status}")
    }
    Ok(())
}

/// The caller owns discovery and credentials; this transport never retries an
/// uncertain mutation or prints internal lifecycle responses.
pub(crate) fn post(
    peer: &mut crate::runtime_identity::VerifiedPeer,
    credential: &str,
    body: &[u8],
) -> Result<(u16, serde_json::Value)> {
    if credential.is_empty() || credential.contains(['\r', '\n']) {
        bail!("invalid credential file")
    }
    if body.len() > 256 * 1024 {
        bail!("request exceeds 256 KiB")
    }
    let address = peer.address;
    let stream = &mut peer.stream;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(stream,"POST /v2/tools HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nAuthorization: Bearer {credential}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",body.len())?;
    stream.write_all(body)?;
    let (status, body) = crate::runtime_identity::response(
        stream, Instant::now() + Duration::from_secs(30), 4 * 1024 * 1024,
    ).context("invalid HTTP response; outcome may already be committed, inspect using request_id before retry")?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .context("invalid JSON response; inspect outcome before retry")?;
    Ok((status, value))
}
