use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::Path as FsPath,
};

use crate::cli::{DaemonRecord, StartArgs};
use crate::util::{json_line, output, PROTOCOL, VERSION};

pub(crate) fn daemon_control(a: &StartArgs, result: Result<Value>, human: &str) -> Result<()> {
    match result {
        Ok(value) => {
            output(a.state.json, value, human);
            Ok(())
        }
        Err(error) => {
            if a.state.json {
                json_line(json!({"state":"failed","error":error.to_string()}));
            }
            Err(error)
        }
    }
}
pub(crate) fn start_daemon(a: &StartArgs) -> Result<Value> {
    if a.host != "127.0.0.1" {
        bail!("loopback_only: host must be 127.0.0.1")
    }
    let record_path = a.state.state_dir.join("daemon.json");
    if let Ok(bytes) = fs::read(&record_path) {
        let record: DaemonRecord =
            serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
        if process_alive(record.pid) {
            if record.version != VERSION || record.protocol_major != PROTOCOL {
                bail!("daemon_version_or_protocol_conflict: explicit drain/restart required")
            }
            return Ok(
                json!({"state":"reused","pid":record.pid,"address":record.address,"version":VERSION,"protocolMajor":PROTOCOL}),
            );
        }
    }
    fs::create_dir_all(&a.state.state_dir)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "daemon",
            "run",
            "--state-dir",
            a.state.state_dir.to_str().context("non-utf8 state dir")?,
            "--host",
            &a.host,
            "--port",
            &a.port.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // 沒有 CREATE_NO_WINDOW 時，console-subsystem 的 daemon run 從無 console 的
        // parent（DETACHED_PROCESS）被生出來，Windows 會配一個常駐的空白 console 視窗。
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let address = format!("{}:{}", a.host, a.port);
    for _ in 0..100 {
        if child.try_wait()?.is_some() {
            bail!("daemon_replacement_exited")
        }
        let record = fs::read(&record_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DaemonRecord>(&bytes).ok());
        if let Some(record) = record.filter(|record| {
            record.pid == child.id()
                && record.address == address
                && record.version == VERSION
                && record.protocol_major == PROTOCOL
        }) {
            if process_alive(record.pid) && health_ready(&address) {
                return Ok(
                    json!({"state":"started","pid":child.id(),"address":address,"version":VERSION,"protocolMajor":PROTOCOL}),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("daemon_start_timeout")
}
pub(crate) fn health_ready(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    let _ = stream.write_all(
        format!("GET /v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    );
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.contains("\"status\":\"ready\"")
}
#[cfg(windows)]
mod win_process {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn GetExitCodeProcess(handle: isize, code: *mut u32) -> i32;
        fn TerminateProcess(handle: isize, code: u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const QUERY: u32 = 0x1000;
    const TERMINATE: u32 = 0x0001;
    const STILL_ACTIVE: u32 = 259;
    pub fn alive(pid: u32) -> bool {
        unsafe {
            let handle = OpenProcess(QUERY, 0, pid);
            if handle == 0 {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE
        }
    }
    pub fn terminate(pid: u32) -> bool {
        unsafe {
            let handle = OpenProcess(TERMINATE, 0, pid);
            if handle == 0 {
                return false;
            }
            let ok = TerminateProcess(handle, 0) != 0;
            CloseHandle(handle);
            ok
        }
    }
}
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill -0 在 Linux 和 macOS 都正確；/proc 只有 Linux 有。
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        win_process::alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        pid != 0
    }
}
pub(crate) fn stop_daemon(state_dir: &FsPath) -> Result<bool> {
    let record_path = state_dir.join("daemon.json");
    let Ok(bytes) = fs::read(&record_path) else {
        return Ok(false);
    };
    let record: DaemonRecord =
        serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
    if record.protocol_major != PROTOCOL {
        bail!("protocol_mismatch: explicit compatible runtime required")
    }
    if process_alive(record.pid) {
        #[cfg(unix)]
        {
            let status = std::process::Command::new("kill")
                .args(["-TERM", &record.pid.to_string()])
                .status()?;
            if !status.success() {
                bail!("failed to stop recorded daemon")
            }
        }
        #[cfg(windows)]
        {
            if !win_process::terminate(record.pid) && process_alive(record.pid) {
                bail!("failed to stop recorded daemon")
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            bail!("daemon stop requires the platform current-user process adapter")
        }
    }
    for _ in 0..100 {
        if !process_alive(record.pid) && TcpStream::connect(&record.address).is_err() {
            let _ = fs::remove_file(record_path);
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("daemon_stop_timeout_or_listener_still_accepting")
}
