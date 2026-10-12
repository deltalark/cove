//! End to end through the real binaries with the dev (`host`) backend: server, CLI,
//! guest agent, streaming, reattach, files and port forwarding. No hypervisor needed,
//! so it runs anywhere; real VM boots are covered by `scripts/smoke-vm.sh`.
//!
//! Needs the workspace built first (`cargo build --workspace`) so `cove-guest` sits
//! next to `cove`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    home: tempfile::TempDir,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Stop any VMs the test left running, then the server.
        if let Ok(rd) = std::fs::read_dir(self.home.path().join("vms")) {
            for e in rd.flatten() {
                if let Ok(text) = std::fs::read_to_string(e.path().join("run.json")) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(pid) = v["pid"].as_i64() {
                            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        }
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cove"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_server() -> Server {
    start_server_with(&[])
}

fn start_server_with(env: &[(&str, &str)]) -> Server {
    let guest = bin().parent().unwrap().join("cove-guest");
    assert!(
        guest.is_file(),
        "build the workspace first: cargo build --workspace ({} missing)",
        guest.display()
    );
    let home = tempfile::tempdir().unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let child = Command::new(bin())
        .args(["serve", "--listen", &listen])
        .env("COVE_HOME", home.path())
        .env("COVE_ALLOW_HOST_BACKEND", "1")
        .envs(env.iter().copied())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server {
        child,
        home,
        url: format!("http://{listen}"),
    }
}

impl Server {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env("COVE_HOME", self.home.path())
            .env("COVE_URL", &self.url)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cove {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run `cove args...` with `input` on stdin and return its output.
    fn piped(&self, args: &[&str], input: Vec<u8>) -> Output {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Write from another thread so a full stdout pipe can't deadlock us.
        let writer = std::thread::spawn(move || {
            stdin.write_all(&input).unwrap();
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        out
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }
}

#[test]
fn vm_lifecycle_exec_and_files() {
    let s = start_server();
    let vm = s.json(&["up", "t1", "--backend", "host"]);
    assert_eq!(vm["state"], "running");
    assert_eq!(vm["backend"], "host");

    // Exit codes and both output streams come through.
    let out = s.run(&[
        "exec",
        "t1",
        "--",
        "sh",
        "-c",
        "echo out; echo err >&2; exit 7",
    ]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");

    // Piped stdin reaches the process.
    let mut child = s
        .cmd(&["exec", "t1", "--", "tr", "a-z", "A-Z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"shout\n").unwrap();
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(text, "SHOUT\n");

    // A missing working folder is reported as such.
    let out = s.run(&["exec", "t1", "--cwd", "/definitely/not/here", "--", "true"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a folder"));

    // Files round-trip, keeping the mode.
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("in.sh");
    std::fs::write(&local, "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(&local, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let remote = dir.path().join("copied.sh");
    s.ok(&[
        "push",
        "t1",
        local.to_str().unwrap(),
        remote.to_str().unwrap(),
    ]);
    assert_eq!(
        s.ok(&["exec", "t1", "--", remote.to_str().unwrap()]),
        "ran\n"
    );
    let back = dir.path().join("back.sh");
    s.ok(&[
        "pull",
        "t1",
        remote.to_str().unwrap(),
        back.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::read_to_string(&back).unwrap(),
        "#!/bin/sh\necho ran\n"
    );

    // Stop, start again, delete.
    assert_eq!(s.json(&["stop", "t1"])["state"], "stopped");
    assert_eq!(s.json(&["start", "t1"])["state"], "running");
    // exec starts a stopped VM by itself.
    s.ok(&["stop", "t1"]);
    assert_eq!(s.ok(&["exec", "t1", "--", "echo", "back"]), "back\n");
    s.ok(&["rm", "t1"]);
    assert_eq!(s.json(&["ls"]), serde_json::json!([]));
}

#[test]
fn spawned_processes_survive_disconnect_and_replay() {
    let s = start_server();
    s.ok(&["up", "t2", "--backend", "host"]);
    s.ok(&[
        "spawn",
        "t2",
        "job",
        "--",
        "sh",
        "-c",
        "echo one; sleep 0.4; echo two; sleep 0.4; echo three",
    ]);
    // The spawn command returned straight away; the process keeps going without us.
    std::thread::sleep(Duration::from_millis(1500));
    let procs = s.json(&["procs", "t2"]);
    assert_eq!(procs[0]["name"], "job");
    assert_eq!(procs[0]["running"], false);
    assert_eq!(procs[0]["exit_code"], 0);
    // Reattaching replays everything; `--from` skips what we already saw.
    assert_eq!(s.ok(&["attach", "t2", "job"]), "one\ntwo\nthree\n");
    assert_eq!(
        s.ok(&["attach", "t2", "job", "--from", "4"]),
        "two\nthree\n"
    );
    // Names are unique while running.
    s.ok(&["spawn", "t2", "long", "--", "sleep", "30"]);
    let dup = s.run(&["spawn", "t2", "long", "--", "sleep", "1"]);
    assert!(String::from_utf8_lossy(&dup.stderr).contains("already running"));
    s.ok(&["kill", "t2", "long"]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        s.json(&["procs", "t2"])
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "long")
            .unwrap()["running"],
        false
    );
}

#[test]
fn ports_are_listed_and_forwarded() {
    if !Path::new("/proc/net/tcp").exists() {
        return; // Port discovery reads /proc; the dev backend on macOS can't list ports.
    }
    let s = start_server();
    s.ok(&["up", "t3", "--backend", "host"]);
    let port = free_port();
    let script = format!(
        "import socket\ns=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1',{port})); s.listen()\nwhile True:\n c,_=s.accept(); c.recv(100); c.sendall(b'pong'); c.close()\n"
    );
    s.ok(&["spawn", "t3", "srv", "--", "python3", "-c", &script]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ports = s.json(&["ports", "t3"]);
        if ports["listening"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["port"] == port)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "port {port} never showed up: {ports}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let f = s.json(&["forward", "t3", &port.to_string()]);
    let url = f["url"].as_str().unwrap();
    let addr = url.trim_start_matches("http://");
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    conn.write_all(b"ping").unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "pong");
    // Asking again reuses the same forward.
    assert_eq!(s.json(&["forward", "t3", &port.to_string()])["url"], url);
}

#[test]
fn api_needs_the_key() {
    let s = start_server();
    let out = Command::new(bin())
        .args(["ls"])
        .env("COVE_URL", &s.url)
        .env("COVE_API_KEY", "wrong")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wrong API key"));
}

#[test]
fn exec_can_create_and_start_on_demand() {
    let s = start_server();
    // Missing VM without --create is an error.
    assert!(!s.run(&["exec", "t5", "--", "true"]).status.success());
    let out = s.run(&[
        "exec",
        "t5",
        "--create",
        "--backend",
        "host",
        "--",
        "echo",
        "made",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "made\n");
    // A stopped VM is started again.
    s.ok(&["stop", "t5"]);
    assert_eq!(
        s.ok(&["exec", "t5", "--create", "--", "echo", "again"]),
        "again\n"
    );
}

#[test]
fn piped_stdin_reaches_exec_and_closes() {
    let s = start_server();
    s.ok(&["up", "t6", "--backend", "host"]);

    // `echo hi | cove exec vm -- cat`: cat only exits once it sees EOF.
    let out = s.piped(&["exec", "t6", "--", "cat"], b"hi\n".to_vec());
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hi\n");

    // Binary data passes through unmodified, as git's ext:: transport needs. More
    // than one pipe buffer each way, and every byte value.
    let data: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let out = s.piped(&["exec", "t6", "--", "cat"], data.clone());
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), data.len());
    assert!(out.stdout == data, "binary data changed on the way");

    // More output than the guest's replay buffer (16 MB), read late, arrives whole.
    let child = s
        .cmd(&["exec", "t6", "--", "head", "-c", "20000000", "/dev/zero"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), 20_000_000);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Replies arrive while stdin is still open, as in a git protocol exchange.
    let mut child = s
        .cmd(&[
            "exec",
            "t6",
            "--",
            "sh",
            "-c",
            "read a; echo got $a; read b; echo got $b",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    stdin.write_all(b"one\n").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
    assert_eq!(line, "got one\n");
    stdin.write_all(b"two\n").unwrap();
    drop(stdin);
    line.clear();
    std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
    assert_eq!(line, "got two\n");
    assert!(child.wait().unwrap().success());
}

#[test]
fn network_policy_and_idle_stop_are_kept() {
    let s = start_server();
    let vm = s.json(&[
        "up",
        "t7",
        "--backend",
        "host",
        "--network",
        "off",
        "--idle-stop",
        "15",
    ]);
    assert_eq!(vm["network"], "off");
    assert_eq!(vm["idle_stop_minutes"], 15);
    let ls = s.ok(&["ls"]);
    assert!(ls.contains("NET"), "{ls}");
    assert!(ls.contains(" off "), "{ls}");
    assert!(ls.contains("15m"), "{ls}");
    assert!(s.ok(&["show", "t7"]).contains("\"network\": \"off\""));

    // The default is full, with no idle timer.
    let vm = s.json(&["up", "t8", "--backend", "host"]);
    assert_eq!(vm["network"], "full");
    assert!(vm.get("idle_stop_minutes").is_none());

    // A bad policy is refused by the CLI.
    assert!(!s
        .run(&["up", "t9", "--backend", "host", "--network", "some"])
        .status
        .success());

    // `--create` passes them on.
    s.ok(&[
        "exec",
        "t10",
        "--create",
        "--backend",
        "host",
        "--network",
        "off",
        "--idle-stop",
        "30",
        "--",
        "true",
    ]);
    let vm = s.json(&["show", "t10"]);
    assert_eq!(vm["network"], "off");
    assert_eq!(vm["idle_stop_minutes"], 30);

    // Copies keep them.
    s.ok(&["stop", "t7"]);
    let copy = s.json(&["copy", "t7", "t7-copy"]);
    assert_eq!(copy["network"], "off");
    assert_eq!(copy["idle_stop_minutes"], 15);
}

#[test]
fn idle_vms_are_stopped() {
    // A "minute" lasts one second here, checked every second.
    let s = start_server_with(&[
        ("COVE_IDLE_CHECK_SECS", "1"),
        ("COVE_IDLE_MINUTE_SECS", "1"),
    ]);
    s.ok(&["up", "idle", "--backend", "host", "--idle-stop", "2"]);
    s.ok(&["up", "busy", "--backend", "host"]);
    // A running named process keeps a VM up past its idle time.
    s.ok(&["spawn", "idle", "job", "--", "sleep", "4"]);
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(s.json(&["show", "idle"])["state"], "running");
    // Once it ends and nothing else happens, the VM stops.
    let deadline = Instant::now() + Duration::from_secs(15);
    while s.json(&["show", "idle"])["state"] != "stopped" {
        assert!(Instant::now() < deadline, "the idle VM was never stopped");
        std::thread::sleep(Duration::from_millis(200));
    }
    // A VM without an idle timer is left alone.
    assert_eq!(s.json(&["show", "busy"])["state"], "running");
}

#[test]
fn health_and_version_report_features() {
    let s = start_server();
    let v = s.json(&["version"]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    let features = v["features"].as_array().unwrap();
    for f in ["network", "idle-stop", "spawn", "attach", "forward", "copy"] {
        assert!(features.iter().any(|x| x == f), "{f} missing: {v}");
    }
    let text = s.ok(&["version"]);
    assert!(text.contains("features: network"), "{text}");

    // Health needs no key.
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["--json", "version"])
        .env("COVE_HOME", home.path())
        .env("COVE_URL", &s.url)
        .env_remove("COVE_API_KEY")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);

    // Without a server, only the client's version.
    let out = Command::new(bin())
        .args(["--json", "version"])
        .env("COVE_HOME", home.path())
        .env("COVE_URL", format!("http://127.0.0.1:{}", free_port()))
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })
    );
}

/// Drives `cove mcp` over stdio, one JSON-RPC request at a time.
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    id: u64,
}

impl Mcp {
    fn start(s: &Server) -> Mcp {
        let mut child = s
            .cmd(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            stdout,
            id: 0,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.id += 1;
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": self.id, "method": method, "params": params
        });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut self.stdout, &mut line).unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.id);
        response["result"].clone()
    }

    /// Call a tool; returns its text and whether it reported an error.
    fn call(&mut self, tool: &str, args: serde_json::Value) -> (String, bool) {
        let result = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        );
        (
            result["content"][0]["text"].as_str().unwrap().to_string(),
            result["isError"] == true,
        )
    }

    fn ok(&mut self, tool: &str, args: serde_json::Value) -> String {
        let (text, error) = self.call(tool, args);
        assert!(!error, "{tool} failed: {text}");
        text
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_tools_for_processes_and_lifecycle() {
    let s = start_server();
    s.ok(&["up", "m1", "--backend", "host"]);
    let mut mcp = Mcp::start(&s);
    let tools = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "vm_spawn", "vm_logs", "vm_kill", "vm_stop", "vm_start", "vm_copy",
    ] {
        assert!(names.contains(&t), "{t} missing from {names:?}");
    }

    // Spawn returns straight away; logs never wait.
    let text = mcp.ok(
        "vm_spawn",
        serde_json::json!({ "name": "m1", "proc": "job",
            "command": "echo first; sleep 0.5; echo second; sleep 30" }),
    );
    assert!(text.contains("Started job"), "{text}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = loop {
        let text = mcp.ok(
            "vm_logs",
            serde_json::json!({ "name": "m1", "proc": "job" }),
        );
        if text.contains("second") {
            break text;
        }
        assert!(Instant::now() < deadline, "no output: {text}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(first.contains("job is running"), "{first}");
    assert!(first.contains("first\nsecond\n"), "{first}");
    assert!(first.contains("Next from: 13"), "{first}");
    // From the end, nothing new, and it still returns at once.
    let started = Instant::now();
    let text = mcp.ok(
        "vm_logs",
        serde_json::json!({ "name": "m1", "proc": "job", "from": 13 }),
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(text.contains("output bytes 13..13"), "{text}");
    let text = mcp.ok(
        "vm_logs",
        serde_json::json!({ "name": "m1", "proc": "job", "from": 6 }),
    );
    assert!(text.ends_with("\nsecond\n"), "{text}");

    mcp.ok(
        "vm_kill",
        serde_json::json!({ "name": "m1", "proc": "job" }),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = mcp.ok(
            "vm_logs",
            serde_json::json!({ "name": "m1", "proc": "job" }),
        );
        if text.contains("job was killed") {
            break;
        }
        assert!(Instant::now() < deadline, "still running: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (text, error) = mcp.call(
        "vm_logs",
        serde_json::json!({ "name": "m1", "proc": "nope" }),
    );
    assert!(error && text.contains("no process named"), "{text}");

    // Copy needs a stopped VM.
    let (text, error) = mcp.call("vm_copy", serde_json::json!({ "name": "m1", "to": "m2" }));
    assert!(error && text.contains("stop"), "{text}");
    assert!(mcp
        .ok("vm_stop", serde_json::json!({ "name": "m1" }))
        .contains("stopped"));
    mcp.ok("vm_copy", serde_json::json!({ "name": "m1", "to": "m2" }));
    assert!(mcp
        .ok("vm_start", serde_json::json!({ "name": "m2" }))
        .contains("running"));
    let text = mcp.ok(
        "vm_exec",
        serde_json::json!({ "name": "m2", "command": "echo copied" }),
    );
    assert!(text.contains("copied"), "{text}");
}
