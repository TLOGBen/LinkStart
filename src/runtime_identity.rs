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
    Reusable(VerifiedPeer),
    Stale,
}

/// Owns the exact connection whose unauthenticated identity was checked.
/// Sensitive callers may reuse this stream, but must never reconnect it.
pub(crate) struct VerifiedPeer {
    pub(crate) address: SocketAddr,
    pub(crate) stream: TcpStream,
}

/// Read exactly one bounded, persistent HTTP response without consuming any
/// following response or waiting for EOF. Ambiguous framing fails closed.
pub(crate) fn response(
    stream: &mut TcpStream,
    deadline: Instant,
    limit: usize,
) -> Result<(u16, Vec<u8>)> {
    fn read(stream: &mut TcpStream, deadline: Instant, bytes: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < bytes.len() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("HTTP response deadline exceeded")?;
            stream.set_read_timeout(Some(remaining))?;
            let count = stream
                .read(&mut bytes[filled..])
                .context("incomplete HTTP response")?;
            if count == 0 {
                bail!("incomplete HTTP response")
            }
            filled += count;
        }
        Ok(())
    }
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() >= limit.min(16 * 1024) {
            bail!("HTTP response headers too large")
        }
        let mut byte = [0];
        read(stream, deadline, &mut byte)?;
        headers.push(byte[0]);
    }
    let text = std::str::from_utf8(&headers).context("invalid HTTP headers")?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let mut status_line = lines.next().context("missing HTTP status")?.splitn(3, ' ');
    if status_line.next() != Some("HTTP/1.1") {
        bail!("persistent HTTP/1.1 response required")
    }
    let status_text = status_line.next().context("missing HTTP status")?;
    if status_text.len() != 3 || !status_text.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid HTTP status")
    }
    let status = status_text.parse::<u16>()?;
    let mut length = None;
    for line in lines {
        let (name, value) = line.split_once(':').context("malformed HTTP header")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            bail!("malformed HTTP header name")
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            bail!("Transfer-Encoding is unsupported")
        }
        if name.eq_ignore_ascii_case("connection")
            && value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("close"))
        {
            bail!("peer cannot preserve the verified connection")
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                bail!("missing or ambiguous Content-Length")
            }
            length = Some(value.parse::<usize>()?);
        }
    }
    let length = length.context("missing Content-Length")?;
    if length > limit.saturating_sub(headers.len()) {
        bail!("HTTP response too large")
    }
    let mut body = vec![0; length];
    // Bound each read as well as the total response duration.
    for chunk in body.chunks_mut(2048) {
        read(stream, deadline, chunk)?;
    }
    Ok((status, body))
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
        "GET /v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: keep-alive\r\n\r\n"
    )
    .context("runtime_identity_unknown: health request failed")?;
    let (status, body) = response(&mut stream, deadline, 16 * 1024)
        .context("runtime_identity_unknown: invalid health response")?;
    if status != 200 {
        bail!("runtime_identity_unknown: health HTTP status is not 200")
    }
    let health: serde_json::Value =
        serde_json::from_slice(&body).context("runtime_identity_unknown: malformed health JSON")?;
    if health["status"] != "ready"
        || health["pid"].as_u64() != Some(record.pid as u64)
        || health["address"].as_str() != Some(record.address.as_str())
        || health["version"].as_str() != Some(record.version.as_str())
        || health["protocolMajor"].as_str() != Some(record.protocol_major.as_str())
        || health["instanceId"].as_str() != Some(instance)
    {
        bail!("runtime_identity_conflict: health does not match recorded Runtime incarnation")
    }
    Ok(Inspection::Reusable(VerifiedPeer { address, stream }))
}

pub(crate) fn require(record: &DaemonRecord) -> Result<VerifiedPeer> {
    match inspect(record)? {
        Inspection::Reusable(address) => Ok(address),
        Inspection::Stale => bail!("runtime_identity_unknown: recorded Runtime has exited"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn connected() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn framed_responses_leave_following_response_on_persistent_peer() {
        let (mut client, mut server) = connected();
        server.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n[]").unwrap();
        // Server remains open throughout both reads: no EOF-based completion.
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            response(&mut client, deadline, 1024).unwrap(),
            (200, b"{}".to_vec())
        );
        assert_eq!(
            response(&mut client, deadline, 1024).unwrap(),
            (201, b"[]".to_vec())
        );
    }

    #[test]
    fn ambiguous_unbounded_and_nonpersistent_responses_fail_closed() {
        for raw in [
            "HTTP/1.1 200 OK\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\ncontent-length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 2,2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999999\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive, close\r\n\r\n{}",
            "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 20 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\n Content-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}",
        ] {
            let (mut client, mut server) = connected();
            server.write_all(raw.as_bytes()).unwrap();
            drop(server);
            assert!(
                response(&mut client, Instant::now() + Duration::from_secs(1), 1024).is_err(),
                "accepted ambiguous response: {raw}"
            );
        }
    }

    #[test]
    fn body_drip_cannot_extend_total_response_deadline() {
        let (mut client, mut server) = connected();
        let sender = thread::spawn(move || {
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n")
                .unwrap();
            for _ in 0..20 {
                if server.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let began = Instant::now();
        assert!(response(&mut client, began + Duration::from_millis(90), 1024).is_err());
        assert!(began.elapsed() < Duration::from_millis(300));
        drop(client);
        sender.join().unwrap();
    }
}
