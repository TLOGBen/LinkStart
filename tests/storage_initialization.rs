use linkstart::runtime_core::{owner_token_path, Engine};
#[cfg(windows)]
fn ps(script: &str, path: &std::path::Path) {
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env_remove("PSModulePath")
        .env("ACL_TEST_PATH", path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
#[cfg(windows)]
fn rejects_existing_other_sid_explicit_read_without_changing_secret() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state 空間");
    drop(Engine::open(&state).unwrap());
    let token = owner_token_path(&state);
    let original = std::fs::read(&token).unwrap();
    ps("$ErrorActionPreference='Stop'; $p=$env:ACL_TEST_PATH; $a=[System.IO.File]::GetAccessControl($p,[System.Security.AccessControl.AccessControlSections]::Access); $s=[System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'); $r=[System.Security.AccessControl.FileSystemAccessRule]::new($s,'Read','Allow'); $a.AddAccessRule($r); [System.IO.File]::SetAccessControl($p,$a)", &token);
    assert!(
        Engine::open(&state).is_err(),
        "must reject untrusted explicit read ACE"
    );
    assert_eq!(std::fs::read(&token).unwrap(), original);
}

#[test]
fn interrupted_first_token_write_without_database_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(owner_token_path(&state), b"short").unwrap();
    let opened = Engine::open(&state)
        .expect("interrupted first token write must recover before any DB exists");
    let owner = std::fs::read_to_string(owner_token_path(&state)).unwrap();
    assert!(owner.len() >= 32);
    assert!(opened.is_owner(&owner));
    drop(opened);
    assert!(Engine::open(&state).unwrap().is_owner(&owner));
}
#[test]
fn existing_database_never_rotates_missing_or_corrupt_token() {
    let dir = tempfile::tempdir().unwrap();
    drop(Engine::open(dir.path()).unwrap());
    let token = owner_token_path(dir.path());
    let original = std::fs::read(&token).unwrap();
    for damaged in [b"".as_slice(), b"short".as_slice()] {
        std::fs::write(&token, damaged).unwrap();
        assert!(Engine::open(dir.path()).is_err());
        assert_eq!(std::fs::read(&token).unwrap(), damaged);
    }
    std::fs::remove_file(&token).unwrap();
    assert!(Engine::open(dir.path()).is_err());
    assert!(!token.exists());
    std::fs::write(&token, &original).unwrap();
    assert!(Engine::open(dir.path())
        .unwrap()
        .is_owner(std::str::from_utf8(&original).unwrap()));
}

// Spawn this integration test binary as a separate initializer process.
#[test]
fn initializer_process() {
    let Some(path) = std::env::var_os("LINKSTART_INIT_TEST_DIR") else {
        return;
    };
    let state = std::path::PathBuf::from(path);
    if std::env::var_os("LINKSTART_INIT_CRASH_STAGE").is_some() {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("v2-initialize.lock"))
            .unwrap();
        lock.lock().unwrap();
        let staged = state.join("v2-owner.token.pending-crashed");
        std::fs::write(&staged, b"incomplete").unwrap();
        std::fs::write(state.join("child-ready"), b"ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let engine = Engine::open(&state).unwrap();
    let owner = std::fs::read_to_string(owner_token_path(&state)).unwrap();
    assert!(engine.is_owner(&owner));
    let request = linkstart::runtime_core::ToolRequest {
        request_id: "concurrent-space".into(),
        tool: "spaces.create".into(),
        space_id: None,
        args: serde_json::json!({"name":"Concurrent", "administrator_name":"Local human"}),
    };
    let result = engine.execute(&owner, &request).unwrap();
    assert!(result["space"]["id"].is_string());
}
// Ensure an assertion/timeout cannot leave the staged crash worker behind.
struct TestChild(std::process::Child);
impl std::ops::Deref for TestChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for TestChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(state: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "initializer_process", "--nocapture"])
        .env("LINKSTART_INIT_TEST_DIR", state);
    command
}
#[test]
fn killed_initializer_and_concurrent_processes_recover_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    // Model pre-publication filesystem state in a new disposable store.
    // The competing children use the real public Engine::open boundary.
    let state = dir.path().join("fresh");
    std::fs::create_dir(&state).unwrap();
    let mut crashed = TestChild(
        child(&state)
            .env("LINKSTART_INIT_CRASH_STAGE", "1")
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !state.join("child-ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not stage token"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Initializers must wait for the writer, then recover after its OS lock is released.
    let mut children = (0..3)
        .map(|_| TestChild(child(&state).spawn().unwrap()))
        .collect::<Vec<_>>();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(children.iter_mut().all(|c| c.try_wait().unwrap().is_none()));
    assert!(
        !owner_token_path(&state).exists(),
        "must not publish while another initializer owns the lock"
    );
    assert!(!state.join("v2.sqlite3").exists());
    crashed.kill().unwrap();
    crashed.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while children.iter_mut().any(|c| c.try_wait().unwrap().is_none()) {
        if let Ok(bytes) = std::fs::read(owner_token_path(&state)) {
            assert_eq!(
                bytes.len(),
                72,
                "canonical token must only expose the complete publication"
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "initializers did not converge"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(
        std::fs::read(state.join("v2-owner.token.pending-crashed")).unwrap(),
        b"incomplete"
    );
    let owner = std::fs::read_to_string(owner_token_path(&state)).unwrap();
    assert!(Engine::open(&state).unwrap().is_owner(&owner));
}

#[test]
#[cfg(windows)]
fn new_storage_acl_is_exclusive_and_existing_database_acl_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("valid 空間");
    drop(Engine::open(&state).unwrap());
    ps("$ErrorActionPreference='Stop'; $u=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; foreach($p in @($env:ACL_TEST_PATH,(Join-Path $env:ACL_TEST_PATH 'v2-owner.token'))) { $a=Get-Acl -LiteralPath $p; if(!$a.AreAccessRulesProtected){throw 'inheriting DACL'}; foreach($r in $a.GetAccessRules($true,$true,[System.Security.Principal.SecurityIdentifier])) { if($r.AccessControlType -eq 'Allow' -and $r.IdentityReference.Value -notin @($u,'S-1-5-18')){throw 'untrusted SID'} } }", &state);
    let db = state.join("v2.sqlite3");
    ps("$ErrorActionPreference='Stop'; $p=$env:ACL_TEST_PATH; $a=[System.IO.File]::GetAccessControl($p,[System.Security.AccessControl.AccessControlSections]::Access); $s=[System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'); $a.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($s,'Read','Allow')); [System.IO.File]::SetAccessControl($p,$a)", &db);
    assert!(Engine::open(&state).is_err());
}
#[test]
#[cfg(windows)]
fn rejects_other_sid_directory_before_initializing() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("foreign-grant");
    std::fs::create_dir(&state).unwrap();
    ps("$ErrorActionPreference='Stop'; $p=$env:ACL_TEST_PATH; $a=[System.IO.Directory]::GetAccessControl($p,[System.Security.AccessControl.AccessControlSections]::Access); $s=[System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'); $a.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($s,'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow')); [System.IO.Directory]::SetAccessControl($p,$a)", &state);
    assert!(Engine::open(&state).is_err());
    assert!(!owner_token_path(&state).exists());
    assert!(!state.join("v2.sqlite3").exists());
}
