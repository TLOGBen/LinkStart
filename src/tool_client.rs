//! A thin public CLI adapter. All authorization and mutation stays in the daemon.
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};
pub(crate) fn run(a: crate::cli::ToolArgs) -> Result<()> {
    let read_credential = || -> Result<String> {
        let credential = std::fs::read_to_string(&a.credential_file)?;
        let credential = credential.trim();
        if credential.is_empty() || credential.contains(['\r', '\n']) {
            bail!("invalid credential file")
        }
        Ok(credential.to_string())
    };
    let (mut peer, credential) = if let Some(address) = a.address {
        if !address.ip().is_loopback() {
            bail!("--address must be a loopback address")
        }
        // Sandboxed agents cannot read discovery, so the Runtime must first
        // prove it already knows this credential before it is sent.
        let credential = read_credential()?;
        let stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut peer = crate::runtime_identity::VerifiedPeer { address, stream };
        prove_runtime(&mut peer, &credential)?;
        (peer, credential)
    } else {
        // The credential is opened only after discovery identity is verified.
        let bytes =
            crate::discovery::read(&a.state_dir)?.context("daemon discovery record missing")?;
        let record: crate::cli::DaemonRecord = serde_json::from_slice(&bytes)?;
        let peer = crate::runtime_identity::require(&record)?;
        (peer, read_credential()?)
    };
    let credential = credential.as_str();
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

/// Fails, without sending the credential, unless the Runtime at this address
/// answers a fresh nonce with the proof only a holder of the credential's hash
/// at this address can compute.
fn prove_runtime(peer: &mut crate::runtime_identity::VerifiedPeer, credential: &str) -> Result<()> {
    use linkstart::runtime_core::{credential_key_id, runtime_proof};
    let nonce: String = (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "key_id": credential_key_id(credential),
        "nonce": nonce,
    }))?;
    let address = peer.address;
    let stream = &mut peer.stream;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let unverified = "runtime_identity_unverified: the process at --address did not prove it is the Runtime that issued this credential; credential not sent";
    write!(stream,"POST /v2/prove HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",body.len()).context(unverified)?;
    stream.write_all(&body).context(unverified)?;
    let (status, body) = crate::runtime_identity::response(
        stream,
        Instant::now() + Duration::from_secs(10),
        64 * 1024,
    )
    .context(unverified)?;
    let answered = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["proof"].as_str().map(str::to_string))
        .unwrap_or_default();
    let expected = runtime_proof(credential, &address.to_string(), &nonce);
    let same = answered.len() == expected.len()
        && answered
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b))
            == 0;
    if status == 404 {
        bail!("runtime_identity_unverified: the Runtime at --address does not recognise this credential (revoked, expired, an invite already used, or a different Runtime); credential not sent")
    }
    if status != 200 || !same {
        bail!(unverified)
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
