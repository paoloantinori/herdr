//! Session restore must not wait for optional repository metadata.
#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn restore_request(socket_path: &Path, method: &str) -> serde_json::Value {
    restore_ok(socket_path, method, serde_json::json!({}))
}

fn restore_request_with_params(
    socket_path: &Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({"id": "restore", "method": method, "params": params})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .unwrap_or_else(|err| panic!("{method} blocked during session restore: {err}"));
    serde_json::from_str(&line).unwrap()
}

fn restore_ok(socket_path: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let response = restore_request_with_params(socket_path, method, params);
    assert!(
        response.get("error").is_none(),
        "{method} failed: {response}"
    );
    response
}

struct RestartEnv {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    data_dir: PathBuf,
}

struct ServerGuard {
    base: PathBuf,
    socket_path: PathBuf,
    child: Option<std::process::Child>,
}

impl ServerGuard {
    fn new(base: &Path, socket_path: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
            socket_path: socket_path.to_path_buf(),
            child: None,
        }
    }

    fn adopt(&mut self, child: std::process::Child) {
        self.child = Some(child);
    }

    fn spawn(&mut self, env: &RestartEnv) {
        self.adopt(spawn_restore_server(env));
    }

    fn stop_current(&mut self, timeout: Duration, what: &str) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(status) = child.try_wait().unwrap() {
            panic!("server exited before server.stop with {status}");
        }
        restore_request(&self.socket_path, "server.stop");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}");
                support::unregister_spawned_herdr_pid(Some(child.id()));
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            support::unregister_spawned_herdr_pid(Some(pid));
        }
        support::cleanup_test_base(&self.base);
    }
}

fn restart_test_env(name: &str) -> RestartEnv {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = PathBuf::from(format!(
        "/tmp/herdr-restart-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&base).unwrap();
    let base = fs::canonicalize(base).unwrap();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let data_dir = config_home.join(if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    });
    support::register_runtime_dir(&runtime_dir);
    RestartEnv {
        base,
        config_home,
        runtime_dir,
        socket_path,
        data_dir,
    }
}

fn spawn_herdr_server(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
) -> std::process::Child {
    let child = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("server")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env("SHELL", "/bin/sh")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_STARTUP_CWD")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    support::register_spawned_herdr_pid(Some(child.id()));
    child
}

const RESTORE_CONFIG: &str = "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\n";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

fn spawn_restore_server(env: &RestartEnv) -> std::process::Child {
    fs::create_dir_all(&env.data_dir).unwrap();
    fs::create_dir_all(&env.runtime_dir).unwrap();
    fs::write(env.data_dir.join("config.toml"), RESTORE_CONFIG).unwrap();
    spawn_herdr_server(&env.config_home, &env.runtime_dir, &env.socket_path)
}

fn wait_for_agent_result(
    socket_path: &Path,
    target: &str,
    timeout: Duration,
    failure: &str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let response = restore_request_with_params(
            socket_path,
            "agent.get",
            serde_json::json!({"target": target}),
        );
        if response.get("result").is_some() {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{failure}: {response}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn create_workspace_pane(env: &RestartEnv) -> String {
    let created = restore_ok(
        &env.socket_path,
        "workspace.create",
        serde_json::json!({"cwd": env.base, "focus": true}),
    );
    created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string()
}

struct FakePi {
    started_marker: PathBuf,
    session_log: PathBuf,
    script: PathBuf,
}

fn write_fake_pi(base: &Path) -> FakePi {
    use std::os::unix::fs::PermissionsExt;

    let pi = FakePi {
        started_marker: base.join("agent-started"),
        session_log: base.join("pi-session.jsonl"),
        script: base.join("pi"),
    };
    fs::write(
        &pi.script,
        format!(
            "#!/bin/sh\nexport HERDR_AGENT=pi\necho started > {}\n/bin/sleep 30\n:\n",
            pi.started_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pi.script, fs::Permissions::from_mode(0o755)).unwrap();
    pi
}

fn drive_fake_pi_agent(socket_path: &Path, pane_id: &str, pi: &FakePi) {
    restore_ok(
        socket_path,
        "pane.send_input",
        serde_json::json!({
            "pane_id": pane_id,
            "text": pi.script.display().to_string(),
            "keys": ["Enter"]
        }),
    );
    support::wait_for_file(&pi.started_marker, Duration::from_secs(5));
    restore_ok(
        socket_path,
        "pane.report_agent_session",
        serde_json::json!({
            "pane_id": pane_id,
            "source": "herdr:pi",
            "agent": "pi",
            "seq": 1,
            "agent_session_path": pi.session_log,
            "session_start_source": "startup"
        }),
    );
    restore_ok(
        socket_path,
        "pane.report_agent",
        serde_json::json!({
            "pane_id": pane_id,
            "source": "herdr:pi",
            "agent": "pi",
            "state": "idle",
            "seq": 2,
            "agent_session_path": pi.session_log
        }),
    );
    wait_for_agent_result(
        socket_path,
        pane_id,
        Duration::from_secs(5),
        "agent process was not detected",
    );
}

fn rename_agent(socket_path: &Path, pane_id: &str, name: &str) {
    restore_ok(
        socket_path,
        "agent.rename",
        serde_json::json!({"target": pane_id, "name": name}),
    );
}

fn assert_agent_name_resolves(socket_path: &Path, name: &str, what: &str) {
    let renamed = wait_for_agent_result(socket_path, name, Duration::from_secs(15), what);
    assert_eq!(
        renamed["result"]["agent"]["name"].as_str(),
        Some(name),
        "agent.get resolved the name but did not report it: {renamed}"
    );
}

#[test]
fn pane_renames_survive_daemon_restart() {
    const LABEL: &str = "cookies-extractor";
    let env = restart_test_env("pane-label");
    let mut guard = ServerGuard::new(&env.base, &env.socket_path);

    guard.spawn(&env);
    support::wait_for_socket(&env.socket_path, Duration::from_secs(10));
    let pane_id = create_workspace_pane(&env);
    restore_ok(
        &env.socket_path,
        "pane.rename",
        serde_json::json!({"pane_id": pane_id, "label": LABEL}),
    );
    support::wait_for_file_contains(
        &env.data_dir.join("session.json"),
        LABEL,
        Duration::from_secs(15),
    );

    guard.stop_current(SHUTDOWN_TIMEOUT, "server did not exit after server.stop");

    guard.spawn(&env);
    support::wait_for_socket(&env.socket_path, Duration::from_secs(10));
    let panes = restore_ok(&env.socket_path, "pane.list", serde_json::json!({}));
    let panes = panes["result"]["panes"].as_array().unwrap().clone();
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!(
        panes[0]["label"].as_str(),
        Some(LABEL),
        "pane rename did not survive the daemon restart: {panes:?}"
    );

    guard.stop_current(SHUTDOWN_TIMEOUT, "server did not exit after server.stop");
}

#[test]
fn unmanaged_agent_names_survive_daemon_restart() {
    const NAME: &str = "cookies-extractor";
    let env = restart_test_env("agent-name");
    let pi = write_fake_pi(&env.base);
    let mut guard = ServerGuard::new(&env.base, &env.socket_path);

    guard.spawn(&env);
    support::wait_for_socket(&env.socket_path, Duration::from_secs(10));
    let pane_id = create_workspace_pane(&env);
    drive_fake_pi_agent(&env.socket_path, &pane_id, &pi);
    rename_agent(&env.socket_path, &pane_id, NAME);
    support::wait_for_file_contains(
        &env.data_dir.join("session.json"),
        NAME,
        Duration::from_secs(15),
    );

    guard.stop_current(SHUTDOWN_TIMEOUT, "server did not exit after server.stop");

    guard.spawn(&env);
    support::wait_for_socket(&env.socket_path, Duration::from_secs(10));
    assert_agent_name_resolves(
        &env.socket_path,
        NAME,
        "agent name did not survive the daemon restart",
    );

    guard.stop_current(SHUTDOWN_TIMEOUT, "server did not exit after server.stop");
}

#[test]
fn server_restores_while_git_metadata_is_blocked() {
    use std::os::unix::fs::OpenOptionsExt;

    for saved_membership in [false, true] {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from(format!("/tmp/herdr-restore-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket_path = runtime_dir.join("herdr.sock");
        let data_dir = config_home.join(if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        });
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git/objects")).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let config_path = repo.join(".git/config");
        assert!(Command::new("mkfifo")
            .arg(&config_path)
            .status()
            .unwrap()
            .success());
        // Keep the FIFO open without writing: metadata reads cannot finish until
        // this guard drops, so API readiness must not depend on a lucky delay.
        let _blocked_metadata = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&config_path)
            .unwrap();
        let membership = saved_membership.then(|| {
            serde_json::json!({
                "key": "saved-repo", "label": "saved-space", "repo_root": repo,
                "checkout_path": repo, "is_linked_worktree": false
            })
        });
        let healthy = base.join("healthy");
        assert!(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(&healthy)
            .status()
            .unwrap()
            .success());
        let healthy_membership = serde_json::json!({
            "key": healthy.join(".git"), "label": "healthy", "repo_root": healthy,
            "checkout_path": healthy, "is_linked_worktree": false
        });
        let snapshot = serde_json::json!({
            "version": 3, "active": 0, "selected": 0,
            "workspaces": [{
                "id": "w1", "custom_name": "restored", "identity_cwd": repo,
                "worktree_space": membership,
                "public_pane_numbers": {"7": 3}, "public_tab_numbers": [2],
                "tabs": [{"layout": {"Pane": 7}, "panes": {"7": {"cwd": repo}},
                          "zoomed": false, "focused": 7, "root_pane": 7}]
            }, {
                "id": "w2", "custom_name": "healthy", "identity_cwd": healthy,
                "worktree_space": healthy_membership,
                "tabs": [{"layout": {"Pane": 9}, "panes": {"9": {"cwd": healthy}},
                          "zoomed": false, "focused": 9, "root_pane": 9}]
            }]
        });
        fs::write(data_dir.join("session.json"), snapshot.to_string()).unwrap();
        fs::write(data_dir.join("config.toml"),
            "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\n[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n").unwrap();
        support::register_runtime_dir(&runtime_dir);
        let mut guard = ServerGuard::new(&base, &socket_path);
        guard.adopt(spawn_herdr_server(&config_home, &runtime_dir, &socket_path));
        support::wait_for_socket(&socket_path, Duration::from_secs(5));

        let workspaces = restore_request(&socket_path, "workspace.list");
        let restored = &workspaces["result"]["workspaces"][0];
        assert_eq!(restored["workspace_id"], "w1");
        assert_eq!(restored["label"], "restored");
        assert_eq!(restored["active_tab_id"], "w1:t2");
        assert_eq!(restored["pane_count"], 1);
        if saved_membership {
            for (method, params) in [
                (
                    "worktree.create",
                    serde_json::json!({"workspace_id": "w1", "branch": "blocked"}),
                ),
                (
                    "worktree.open",
                    serde_json::json!({"workspace_id": "w1", "branch": "main"}),
                ),
                ("worktree.list", serde_json::json!({"workspace_id": "w1"})),
                (
                    "worktree.remove",
                    serde_json::json!({"workspace_id": "w1", "force": true}),
                ),
            ] {
                let response = restore_request_with_params(&socket_path, method, params);
                assert_eq!(
                    response["error"]["code"], "worktree_operation_in_progress",
                    "{method}: {response}"
                );
            }
        }
        if saved_membership {
            // A blocked saved membership must not keep unrelated saved
            // memberships pending or prevent their worktree operations.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let response = restore_request_with_params(
                    &socket_path,
                    "worktree.list",
                    serde_json::json!({"workspace_id": "w2"}),
                );
                if response.get("error").is_none() {
                    break;
                }
                assert_eq!(response["error"]["code"], "worktree_operation_in_progress");
                assert!(
                    std::time::Instant::now() < deadline,
                    "healthy workspace validation waited for another checkout's blocked metadata"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        restore_request(&socket_path, "session.snapshot");
        restore_request(&socket_path, "agent.list");
        guard.stop_current(
            Duration::from_secs(5),
            "server shutdown waited for blocked Git metadata",
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(data_dir.join("session.json")).unwrap()).unwrap();
        assert_eq!(
            saved["workspaces"][0]["identity_cwd"],
            snapshot["workspaces"][0]["identity_cwd"]
        );
        assert_eq!(
            saved["workspaces"][0]["worktree_space"],
            snapshot["workspaces"][0]["worktree_space"]
        );
        drop(guard);
    }
}
