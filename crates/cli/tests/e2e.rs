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

/// A real gVisor sandbox through the whole stack. Opt in with `COVE_GVISOR_E2E=1`: it
/// needs root, `runsc` on PATH, a static guest agent in `COVE_GUEST_BINARY`, and
/// network access to Docker Hub for the Debian rootfs.
#[test]
fn gvisor_sandbox_end_to_end() {
    if std::env::var("COVE_GVISOR_E2E").as_deref() != Ok("1") {
        eprintln!("set COVE_GVISOR_E2E=1 to run the gVisor test");
        return;
    }
    let s = start_server();
    s.ok(&["image", "pull", "debian-13"]);
    let share = tempfile::tempdir().unwrap();
    std::fs::write(share.path().join("in.txt"), "from host\n").unwrap();
    let mount = format!("{}:/work", share.path().display());
    let vm = s.json(&[
        "up",
        "gv",
        "--backend",
        "gvisor",
        "--network",
        "off",
        "--cpus",
        "2",
        "--memory",
        "1024",
        "--mount",
        &mount,
    ]);
    assert_eq!(vm["backend"], "gvisor");
    assert_eq!(
        s.ok(&["exec", "gv", "--", "cat", "/etc/debian_version"])
            .trim()
            .split('.')
            .next(),
        Some("13")
    );
    assert_eq!(s.ok(&["exec", "gv", "--", "nproc"]), "2\n");
    assert_eq!(
        s.ok(&["exec", "gv", "--", "cat", "/work/in.txt"]),
        "from host\n"
    );
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    assert!(s.piped(&["exec", "gv", "--", "cat"], data.clone()).stdout == data);
    // The disk persists across a restart, and copies keep it.
    s.ok(&["exec", "gv", "--", "sh", "-c", "echo kept > /root/marker"]);
    s.ok(&["stop", "gv"]);
    s.ok(&["start", "gv"]);
    assert_eq!(s.ok(&["exec", "gv", "--", "cat", "/root/marker"]), "kept\n");
    s.ok(&["stop", "gv"]);
    s.ok(&["copy", "gv", "gv-copy"]);
    s.ok(&["start", "gv-copy"]);
    assert_eq!(
        s.ok(&["exec", "gv-copy", "--", "cat", "/root/marker"]),
        "kept\n"
    );
    s.ok(&["rm", "gv-copy"]);
    s.ok(&["rm", "gv"]);
    assert_eq!(s.json(&["ls"]), serde_json::json!([]));
}

/// A plain HTTP server on this machine that answers every request with its head.
fn echo_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if conn.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    head.len()
                );
                let _ = conn.write_all(reply.as_bytes());
                let _ = conn.write_all(&head);
            });
        }
    });
    port
}

#[test]
fn allowlist_and_secrets_go_through_the_egress_proxy() {
    let upstream = echo_server();
    let guest_port = free_port().to_string();
    let s = start_server_with(&[
        // The dev backend's "VM" is this machine, so let it reach itself, and keep its
        // proxy port away from anything else on this machine.
        ("COVE_EGRESS_ALLOW_LOCAL", "1"),
        ("COVE_EGRESS_GUEST_PORT", &guest_port),
    ]);
    let out = s
        .cmd(&[
            "--json",
            "up",
            "e1",
            "--backend",
            "host",
            "--network",
            "allowlist",
            "--allow",
            "localhost",
            "--secret",
            "TEST_TOKEN@api.example.com",
        ])
        .env("TEST_TOKEN", "real-value-123")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("real-value-123"),
        "the API showed a secret: {text}"
    );
    let vm: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(vm["network"], "allowlist");
    assert_eq!(vm["allow"], serde_json::json!(["localhost"]));
    assert_eq!(vm["secrets"][0]["name"], "TEST_TOKEN");
    assert_eq!(
        vm["secrets"][0]["hosts"],
        serde_json::json!(["api.example.com"])
    );

    // Inside, the secret is a placeholder and the proxy is set for every command.
    let env = s.ok(&[
        "exec",
        "e1",
        "--",
        "sh",
        "-c",
        "echo $TEST_TOKEN; echo $HTTPS_PROXY",
    ]);
    assert!(!env.contains("real-value-123"), "{env}");
    assert!(env.contains("cove"), "{env}");
    assert!(env.contains(&format!("127.0.0.1:{guest_port}")), "{env}");

    // An allowed host answers through the proxy; others are refused.
    let url = format!("http://localhost:{upstream}/hello");
    let got = s.ok(&["exec", "e1", "--", "curl", "-s", "--noproxy", "", &url]);
    assert!(got.contains("GET /hello HTTP/1.1"), "{got}");
    let code = |s: &Server, url: &str| {
        s.ok(&[
            "exec",
            "e1",
            "--",
            "curl",
            "-s",
            "--noproxy",
            "",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            url,
        ])
    };
    assert_eq!(code(&s, "http://example.org/"), "403");

    // Both are in the log.
    let report = s.json(&["egress", "e1"]);
    let log = report["log"].as_array().unwrap();
    assert!(log
        .iter()
        .any(|e| e["host"] == "localhost" && e["allowed"] == true));
    assert!(log
        .iter()
        .any(|e| e["host"] == "example.org" && e["allowed"] == false));
    assert_eq!(report["enforced"], true);

    // A new allowlist applies at once.
    s.ok(&["allow", "e1", "example.com"]);
    assert_eq!(code(&s, &url), "403");
    let shown = s.ok(&["egress", "e1"]);
    assert!(shown.contains("Reaches only: example.com"), "{shown}");
    assert!(shown.contains("BLOCKED"), "{shown}");
    assert!(
        shown.contains("TEST_TOKEN goes only to api.example.com"),
        "{shown}"
    );

    // Bad rules and secrets are refused.
    assert!(!s.run(&["allow", "e1", "http://x"]).status.success());
    let missing = s.run(&[
        "up",
        "e2",
        "--backend",
        "host",
        "--secret",
        "NOT_SET_ANYWHERE_X",
    ]);
    assert!(!missing.status.success());
    let unknown = s
        .cmd(&["up", "e3", "--backend", "host", "--secret", "MY_THING"])
        .env("MY_THING", "x")
        .output()
        .unwrap();
    assert!(
        !unknown.status.success(),
        "a secret with no hosts was accepted"
    );
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("hosts"));
}

/// Wait until `cove show name` has `state`, for up to `secs` seconds.
fn wait_state(s: &Server, name: &str, state: &str, secs: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let vm = s.json(&["show", name]);
        if vm["state"] == state {
            return vm;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never became {state}: {vm}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn pause_resume_and_idle_pause() {
    let s = start_server_with(&[
        ("COVE_IDLE_CHECK_SECS", "1"),
        ("COVE_IDLE_MINUTE_SECS", "1"),
    ]);
    s.ok(&["up", "p1", "--backend", "host"]);
    s.ok(&[
        "spawn",
        "p1",
        "counter",
        "--",
        "sh",
        "-c",
        "i=0; while true; do i=$((i+1)); echo $i > n; sleep 0.1; done",
    ]);
    let vm = s.json(&["pause", "p1"]);
    assert_eq!(vm["state"], "paused");
    assert_eq!(vm["paused_by"], "user");
    // Paused on purpose: the API waits for resume instead of waking for a request
    // (the CLI's exec and shell resume it, as they start a stopped VM).
    let refused = s.run(&["ports", "p1"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("paused"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let vm = s.json(&["resume", "p1"]);
    assert_eq!(vm["state"], "running");
    assert!(vm.get("paused_by").is_none());
    // Its processes carried on where they were.
    let procs = s.json(&["procs", "p1"]);
    assert_eq!(procs[0]["running"], true);
    s.ok(&["kill", "p1", "counter"]);

    // An idle pause wakes up by itself when the VM is used.
    s.ok(&["auto-pause", "p1", "2"]);
    let vm = wait_state(&s, "p1", "paused", 15);
    assert_eq!(vm["paused_by"], "idle");
    assert!(s.ok(&["ls"]).contains("pause 2m"));
    assert_eq!(s.ok(&["exec", "p1", "--", "echo", "awake"]).trim(), "awake");
    assert_eq!(s.json(&["show", "p1"])["state"], "running");

    // `cove sh`'s helper resumes a VM paused on purpose, and stop works on one.
    // `--create` takes an auto-pause timer, as Runo passes it.
    s.ok(&[
        "exec",
        "p2",
        "--create",
        "--backend",
        "host",
        "--auto-pause",
        "20",
        "--",
        "true",
    ]);
    assert_eq!(s.json(&["show", "p2"])["auto_pause_minutes"], 20);
    s.ok(&["auto-pause", "p1", "0"]);
    s.ok(&["pause", "p1"]);
    s.ok(&["exec", "p1", "--create", "--", "true"]);
    s.ok(&["pause", "p1"]);
    assert_eq!(s.json(&["stop", "p1"])["state"], "stopped");
}

#[test]
fn timeouts_labels_and_rename() {
    let s = start_server_with(&[
        ("COVE_IDLE_CHECK_SECS", "1"),
        ("COVE_IDLE_MINUTE_SECS", "1"),
    ]);
    // "2m" lasts two seconds here.
    let vm = s.json(&[
        "up",
        "doomed",
        "--backend",
        "host",
        "--timeout",
        "2m",
        "--on-timeout",
        "delete",
        "--label",
        "task=review",
        "--label",
        "owner=runo",
    ]);
    assert_eq!(vm["timeout"]["action"], "delete");
    assert_eq!(vm["labels"]["task"], "review");
    s.ok(&["up", "keeper", "--backend", "host", "--label", "task=build"]);
    let found = s.json(&["ls", "--label", "task=review"]);
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["name"], "doomed");
    let deadline = Instant::now() + Duration::from_secs(15);
    while s.run(&["show", "doomed"]).status.success() {
        assert!(
            Instant::now() < deadline,
            "the timeout never deleted the VM"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // A pause timeout fires once, then the VM has none.
    s.ok(&["timeout", "keeper", "1m"]);
    let vm = wait_state(&s, "keeper", "paused", 15);
    assert_eq!(vm["paused_by"], "timeout");
    assert!(vm.get("timeout").is_none());
    s.ok(&["resume", "keeper"]);
    s.ok(&["timeout", "keeper", "3h"]);
    s.ok(&["timeout", "keeper", "0"]);
    assert!(s.json(&["show", "keeper"]).get("timeout").is_none());
    assert!(!s.run(&["timeout", "keeper", "2d"]).status.success());

    s.ok(&["label", "keeper", "task=deploy", "--rm", "missing"]);
    assert_eq!(s.json(&["show", "keeper"])["labels"]["task"], "deploy");

    // Rename: stopped only, and the old name is free again.
    assert!(!s.run(&["rename", "keeper", "kept"]).status.success());
    s.ok(&["stop", "keeper"]);
    let vm = s.json(&["rename", "keeper", "kept"]);
    assert_eq!(vm["name"], "kept");
    assert_eq!(vm["labels"]["task"], "deploy");
    assert!(!s.run(&["show", "keeper"]).status.success());
    s.ok(&["start", "kept"]);
}

#[test]
fn snapshots_and_settings() {
    let s = start_server();
    s.ok(&["up", "base", "--backend", "host"]);
    s.ok(&[
        "exec",
        "base",
        "--",
        "sh",
        "-c",
        "echo set-up-once > marker",
    ]);
    // Saved while running: the VM carries on.
    let snap = s.json(&[
        "snapshot",
        "save",
        "base",
        "--name",
        "ready",
        "--description",
        "with marker",
    ]);
    assert_eq!(snap["kind"], "host");
    assert_eq!(snap["source_vm"], "base");
    assert_eq!(s.json(&["show", "base"])["state"], "running");
    assert!(!s
        .run(&["snapshot", "save", "base", "--name", "ready"])
        .status
        .success());

    let vm = s.json(&[
        "up",
        "from-snap",
        "--backend",
        "host",
        "--snapshot",
        "ready",
    ]);
    assert_eq!(vm["snapshot"], "ready");
    assert_eq!(
        s.ok(&["exec", "from-snap", "--", "cat", "marker"]).trim(),
        "set-up-once"
    );
    // Changes in the new VM don't reach the snapshot or the old VM.
    s.ok(&[
        "exec",
        "from-snap",
        "--",
        "sh",
        "-c",
        "echo changed > marker",
    ]);
    assert_eq!(
        s.ok(&["exec", "base", "--", "cat", "marker"]).trim(),
        "set-up-once"
    );
    let listed = s.json(&["snapshot", "ls"]);
    assert_eq!(listed[0]["name"], "ready");
    assert!(!s
        .run(&["up", "x", "--backend", "host", "--snapshot", "missing"])
        .status
        .success());

    // Server settings fill in what a create leaves out.
    let set = s.json(&[
        "settings",
        "set",
        "cpus=3",
        "snapshot=ready",
        "auto_pause_minutes=45",
    ]);
    assert_eq!(set["cpus"], 3);
    let vm = s.json(&["up", "defaults", "--backend", "host"]);
    assert_eq!(vm["cpus"], 3);
    assert_eq!(vm["snapshot"], "ready");
    assert_eq!(vm["auto_pause_minutes"], 45);
    assert_eq!(
        s.ok(&["exec", "defaults", "--", "cat", "marker"]).trim(),
        "set-up-once"
    );
    // What the request says wins.
    let vm = s.json(&[
        "up",
        "explicit",
        "--backend",
        "host",
        "--cpus",
        "1",
        "--image",
        "debian-13",
    ]);
    assert_eq!(vm["cpus"], 1);
    assert!(vm.get("snapshot").is_none());
    assert!(!s.run(&["settings", "set", "cpus=0"]).status.success());
    let unset = s.json(&[
        "settings",
        "unset",
        "snapshot",
        "cpus",
        "auto_pause_minutes",
    ]);
    assert_eq!(unset, serde_json::json!({}));

    s.ok(&["snapshot", "rm", "ready"]);
    assert_eq!(s.json(&["snapshot", "ls"]), serde_json::json!([]));
    // VMs made from it keep working.
    assert_eq!(
        s.ok(&["exec", "from-snap", "--", "cat", "marker"]).trim(),
        "changed"
    );
}

#[test]
fn pairing_gives_a_named_key() {
    let s = start_server();
    let code = s.json(&["pair", "--name", "runo"]);
    let code = code["code"].as_str().unwrap().to_string();
    // Another computer: its own Cove folder, no key of its own yet.
    let other = tempfile::tempdir().unwrap();
    let cli = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("COVE_HOME", other.path())
            .env_remove("COVE_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = cli(&["login", &s.url, &code, "--name", "laptop"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Logged in: it reaches the server with its own key.
    let out = cli(&["ls"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Codes work once.
    assert!(!cli(&["login", &s.url, &code]).status.success());
    let keys = s.json(&["keys", "ls"]);
    assert_eq!(keys[0]["name"], "laptop");
    assert!(keys[0].get("hash").is_none());
    // Revoked, the key stops working.
    s.ok(&["keys", "rm", keys[0]["id"].as_str().unwrap()]);
    assert!(!cli(&["ls"]).status.success());
    assert!(cli(&["logout"]).status.success());

    // Pairing over HTTP directly, as Runo does.
    let code = s.json(&["pair"])["code"].as_str().unwrap().to_string();
    let reply = Command::new("curl")
        .args(["-s", "-X", "POST", "-H", "Content-Type: application/json"])
        .arg("-d")
        .arg(format!(r#"{{"code":"{code}","name":"Runo"}}"#))
        .arg(format!("{}/v1/pair", s.url))
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert!(v["key"].as_str().unwrap().starts_with("cove_"), "{v}");
    assert!(v["server"]["features"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("pair")));
    // An app that keeps the key itself: nothing saved on this computer.
    let code = s.json(&["pair"])["code"].as_str().unwrap().to_string();
    let out = cli(&[
        "--json",
        "login",
        &s.url,
        &code,
        "--name",
        "Runo",
        "--no-save",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["key"].as_str().unwrap().starts_with("cove_"));
    assert!(!other.path().join("login.json").exists());
    let made = s.json(&["keys", "create", "ci"]);
    assert!(made["key"].as_str().unwrap().starts_with("cove_"));
}

#[test]
fn names_from_urls_stay_inside_cove_folders() {
    let s = start_server();
    let key = std::fs::read_to_string(s.home.path().join("api-key")).unwrap();
    std::fs::create_dir_all(s.home.path().join("keep")).unwrap();
    for path in ["/v1/snapshots/..%2Fkeep", "/v1/vms/..%2Fkeep"] {
        let out = Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-X",
                "DELETE",
                "-H",
            ])
            .arg(format!("Authorization: Bearer {}", key.trim()))
            .arg(format!("{}{path}", s.url))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "404", "{path}");
    }
    assert!(s.home.path().join("keep").is_dir());
}

#[test]
fn https_pairing_pins_the_servers_certificate() {
    let port = free_port();
    let https = format!("127.0.0.1:{port}");
    let s = start_server_with(&[("COVE_HTTPS", &https), ("COVE_PUBLIC_HOST", "127.0.0.1")]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&https).is_err() {
        assert!(Instant::now() < deadline, "HTTPS listener did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let v = s.json(&["pair"]);
    let pair = v["pair"].as_str().unwrap().to_string();
    let fp = v["fingerprint"].as_str().unwrap().to_string();
    assert!(pair.starts_with(&format!("cove://127.0.0.1:{port}/")));
    assert!(pair.ends_with(&format!("?fp={fp}")));

    // Another computer: its own home, no COVE_URL.
    let other = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("COVE_HOME", other.path())
            .env_remove("COVE_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    // A wrong fingerprint is refused before the code is sent, so the code still works.
    let code = v["code"].as_str().unwrap();
    let url = format!("https://127.0.0.1:{port}");
    let wrong = format!("sha256:{}", "0".repeat(64));
    let out = run(&["login", &url, code, "--fingerprint", &wrong]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not the"));

    let out = run(&["login", &pair]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let login: serde_json::Value =
        serde_json::from_slice(&std::fs::read(other.path().join("login.json")).unwrap()).unwrap();
    assert_eq!(login["url"], url);
    assert_eq!(login["fingerprint"], fp);

    // Everything goes over the pinned HTTPS now: plain requests and the WebSocket stream.
    let out = run(&["up", "tls1", "--backend", "host"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["exec", "tls1", "--", "echo", "over-tls"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "over-tls");
    let out = run(&["version"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("https"));
    s.ok(&["rm", "tls1"]);
}

/// Gets a real certificate from Pebble, Let's Encrypt's test server, for 127.0.0.1 the
/// way `--domain ip` does on a VPS. Runs when `COVE_PEBBLE` points at a pebble binary
/// and `COVE_PEBBLE_CERTS` at its `test/certs` folder (CI downloads both).
#[test]
fn acme_gets_an_ip_certificate_from_pebble() {
    let (Ok(pebble), Ok(certs)) = (
        std::env::var("COVE_PEBBLE"),
        std::env::var("COVE_PEBBLE_CERTS"),
    ) else {
        eprintln!("skipped: set COVE_PEBBLE and COVE_PEBBLE_CERTS");
        return;
    };
    let certs = PathBuf::from(certs);
    let dir = tempfile::tempdir().unwrap();
    let (acme_port, mgmt_port, https_port) = (free_port(), free_port(), free_port());
    let config = serde_json::json!({ "pebble": {
        "listenAddress": format!("127.0.0.1:{acme_port}"),
        "managementListenAddress": format!("127.0.0.1:{mgmt_port}"),
        "certificate": certs.join("localhost/cert.pem"),
        "privateKey": certs.join("localhost/key.pem"),
        "httpPort": free_port(),
        "tlsPort": https_port,
        "profiles": {
            "default": { "description": "default", "validityPeriod": 7776000 },
            "shortlived": { "description": "short", "validityPeriod": 518400 }
        }
    }});
    let config_path = dir.path().join("pebble.json");
    std::fs::write(&config_path, config.to_string()).unwrap();
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _pebble = Kill(
        Command::new(pebble)
            .arg("-config")
            .arg(&config_path)
            .env("PEBBLE_VA_NOSLEEP", "1")
            .env("PEBBLE_WFE_NONCEREJECT", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", acme_port)).is_err() {
        assert!(Instant::now() < deadline, "pebble did not start");
        std::thread::sleep(Duration::from_millis(50));
    }

    let https = format!("127.0.0.1:{https_port}");
    let directory = format!("https://127.0.0.1:{acme_port}/dir");
    let root = certs.join("pebble.minica.pem");
    let s = start_server_with(&[
        ("COVE_HTTPS", &https),
        ("COVE_PUBLIC_HOST", "127.0.0.1"),
        ("COVE_DOMAIN", "ip"),
        ("COVE_ACME_DIRECTORY", &directory),
        ("COVE_ACME_ROOT", root.to_str().unwrap()),
    ]);
    // Until the certificate arrives the line carries the self-signed fingerprint; after,
    // it doesn't need one.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = s.json(&["pair"]);
        if v["fingerprint"].is_null() {
            assert_eq!(v["pair"].as_str().unwrap().split('?').count(), 1);
            break;
        }
        assert!(Instant::now() < deadline, "no certificate from pebble");
        std::thread::sleep(Duration::from_millis(250));
    }
    let saved = s.home.path().join("tls/acme/127-0-0-1.pem");
    let pem = std::fs::read_to_string(&saved).unwrap();
    assert!(pem.contains("BEGIN CERTIFICATE"));
}

#[test]
fn mcp_over_http_uses_the_callers_key() {
    let s = start_server();
    s.ok(&["up", "mh1", "--backend", "host"]);
    let key = s.json(&["keys", "create", "hosted-agent"])["key"]
        .as_str()
        .unwrap()
        .to_string();
    let post = |key: &str, body: serde_json::Value| {
        let out = Command::new("curl")
            .args(["-s", "--noproxy", "*", "-w", "\n%{http_code}", "-X", "POST"])
            .args(["-H", "Content-Type: application/json"])
            .args(["-H", &format!("Authorization: Bearer {key}")])
            .arg("-d")
            .arg(body.to_string())
            .arg(format!("{}/v1/mcp", s.url))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, status) = text.rsplit_once('\n').unwrap();
        (status.to_string(), body.to_string())
    };
    let call = |id: u32, method: &str, params: serde_json::Value| serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

    let (status, _) = post("wrong", call(1, "initialize", serde_json::json!({})));
    assert_eq!(status, "401");

    let (status, body) = post(&key, call(1, "initialize", serde_json::json!({})));
    assert_eq!(status, "200");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "cove");

    // Notifications get no body.
    let note = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(post(&key, note).0, "202");

    let (_, body) = post(
        &key,
        call(
            2,
            "tools/call",
            serde_json::json!({ "name": "vm_exec", "arguments": { "name": "mh1", "command": "echo from-http-mcp" } }),
        ),
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("from-http-mcp"), "{v}");

    // Batches answer each request.
    let batch = serde_json::json!([
        call(3, "ping", serde_json::json!({})),
        call(4, "tools/list", serde_json::json!({}))
    ]);
    let (_, body) = post(&key, batch);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
    s.ok(&["rm", "mh1"]);
}
