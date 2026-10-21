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
    if let Some(bytes) = crate::discovery::read(&a.state.state_dir)? {
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    let mut child = win_process::spawn_detached(&command)?;
    #[cfg(not(windows))]
    let mut child = command.spawn()?;
    let address = format!("{}:{}", a.host, a.port);
    // Discovery is published only after Runtime initialization and listener
    // binding succeed. Use an elapsed bound instead of poll count: the previous
    // loop also spent up to 100ms per health probe (about 12s in total).
    let result = (|| -> Result<Value> {
        let startup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < startup_deadline {
            if child.try_wait()?.is_some() {
                bail!("daemon_replacement_exited")
            }
            let record = crate::discovery::read(&a.state.state_dir)?
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
    })();
    // Initialization may already own work before discovery is published. A
    // failed acknowledgement cannot authorize killing that process. Preserve
    // its exact identity and recovery locator instead of claiming it stopped.
    result.map_err(|error| anyhow::anyhow!(
        "startup_outcome_unknown: pid={} address={} state_dir={} detail={error}; inspect before retry",
        child.id(), address, a.state.state_dir.display()
    ))
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
        fn CloseHandle(handle: isize) -> i32;
    }
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use std::{ffi::c_void, io, ptr};
    #[repr(C)]
    struct StartupInfo {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_chars: u32,
        y_chars: u32,
        fill: u32,
        flags: u32,
        show: u16,
        reserved_size: u16,
        reserved_bytes: *mut u8,
        stdin: *mut c_void,
        stdout: *mut c_void,
        stderr: *mut c_void,
    }
    #[repr(C)]
    struct ProcessInfo {
        process: *mut c_void,
        thread: *mut c_void,
        pid: u32,
        tid: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateProcessW(
            application: *const u16,
            command: *mut u16,
            process_security: *mut c_void,
            thread_security: *mut c_void,
            inherit_handles: i32,
            flags: u32,
            environment: *mut c_void,
            directory: *const u16,
            startup: *mut StartupInfo,
            info: *mut ProcessInfo,
        ) -> i32;
    }
    pub struct DetachedChild {
        handle: OwnedHandle,
        pid: u32,
    }
    impl DetachedChild {
        pub fn id(&self) -> u32 {
            self.pid
        }
        pub fn try_wait(&mut self) -> io::Result<Option<()>> {
            let mut code = 0;
            // SAFETY: the owned handle remains valid throughout this query.
            if unsafe { GetExitCodeProcess(self.handle.as_raw_handle() as isize, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((code != STILL_ACTIVE).then_some(()))
        }
    }
    // Rust's stable Command API inherits unrelated inheritable handles on Windows.
    // A daemon must not retain the control caller's pipe: launch with inheritance
    // disabled. This helper intentionally supports this fixed command only (no env/cwd overrides).
    pub fn spawn_detached(command: &std::process::Command) -> io::Result<DetachedChild> {
        fn quote(arg: &std::ffi::OsStr, output: &mut Vec<u16>) -> io::Result<()> {
            output.push(34);
            let mut slashes = 0;
            for unit in arg.encode_wide() {
                if unit == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "NUL in daemon argument",
                    ));
                }
                if unit == 92 {
                    slashes += 1;
                    continue;
                }
                output.extend(std::iter::repeat_n(
                    92,
                    if unit == 34 { slashes * 2 + 1 } else { slashes },
                ));
                slashes = 0;
                output.push(unit);
            }
            output.extend(std::iter::repeat_n(92, slashes * 2));
            output.push(34);
            Ok(())
        }
        let mut application: Vec<u16> = command.get_program().encode_wide().collect();
        application.push(0);
        let mut line = Vec::new();
        quote(command.get_program(), &mut line)?;
        for arg in command.get_args() {
            line.push(32);
            quote(arg, &mut line)?;
        }
        line.push(0);
        // SAFETY: both structures are C layouts and all-null is a valid initial
        // state. Application/command buffers remain alive until CreateProcessW returns.
        unsafe {
            let mut startup: StartupInfo = std::mem::zeroed();
            startup.cb = std::mem::size_of::<StartupInfo>() as u32;
            let mut info: ProcessInfo = std::mem::zeroed();
            if CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                0x0800_0000,
                ptr::null_mut(),
                ptr::null(),
                &mut startup,
                &mut info,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let handle = OwnedHandle::from_raw_handle(info.process);
            drop(OwnedHandle::from_raw_handle(info.thread));
            Ok(DetachedChild {
                handle,
                pid: info.pid,
            })
        }
    }
    const QUERY: u32 = 0x1000;
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
}
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // kill -0 also succeeds for a zombie awaiting its parent's wait(). Its
        // process has exited and cannot own a listener or perform more cleanup.
        if fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("State:")
                        .and_then(|state| state.split_whitespace().next())
                        .map(str::to_owned)
                })
            })
            .is_some_and(|state| state == "Z" || state == "X")
        {
            return false;
        }
    }
    #[cfg(unix)]
    {
        // /proc is Linux-only; other Unix platforms retain their native query.
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
    let Some(bytes) = crate::discovery::read(state_dir)? else {
        return Ok(false);
    };
    let record: DaemonRecord =
        serde_json::from_slice(&bytes).context("invalid daemon discovery record")?;
    if record.protocol_major != PROTOCOL {
        bail!("protocol_mismatch: explicit compatible runtime required")
    }
    let address: std::net::SocketAddr = record
        .address
        .parse()
        .context("invalid daemon discovery address")?;
    if !address.ip().is_loopback() {
        bail!("discovery does not name a live loopback daemon")
    }
    if process_alive(record.pid) {
        // Read the existing owner token only. Initializing Engine here could
        // rotate authority while trying to stop a different live instance.
        let credential = fs::read_to_string(linkstart::runtime_core::owner_token_path(state_dir))
            .context("cannot read existing runtime owner credential")?;
        let call = |tool: &str, args: Value| -> Result<Value> {
            let body = serde_json::to_vec(
                &json!({"request_id":uuid::Uuid::new_v4().to_string(),"tool":tool,"args":args}),
            )?;
            let (status, value) = crate::tool_client::post(address, credential.trim(), &body)
                .context("runtime shutdown transport failed; effects may already exist; inspect before retry")?;
            if !(200..300).contains(&status) {
                bail!("runtime shutdown returned HTTP {status}; daemon was not replaced; inspect before retry")
            }
            Ok(value)
        };
        let prepared = call("runtime.prepare_shutdown", json!({}))?;
        let ticket = prepared["ticket"]
            .as_str()
            .filter(|v| !v.is_empty())
            .context("runtime shutdown returned no ticket; inspect before retry")?;
        let confirmed = call("runtime.confirm_shutdown", json!({"ticket":ticket}))?;
        if confirmed["status"] != "stopping" || confirmed["history"] != "durable" {
            bail!("runtime shutdown pending or unconfirmed; effects may already exist; daemon was not replaced; inspect before retry")
        }
    }
    // The runtime schedules its stop signal after 150 ms and bounds remaining
    // SSE transport drain at two seconds. Leave room for both before witnessing
    // process/listener exit; this does not extend or bypass owned cleanup.
    for _ in 0..250 {
        if !process_alive(record.pid)
            && TcpStream::connect_timeout(&address, std::time::Duration::from_millis(100)).is_err()
        {
            // A concurrent start may have published a different discovery record.
            crate::discovery::remove_if_matches(state_dir, &bytes)?;
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    bail!("daemon_stop_timeout_or_listener_still_accepting")
}
