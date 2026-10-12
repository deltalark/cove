//! `cove-guest`: the agent inside every cove VM.
//!
//! It listens on vsock port 1024 (or TCP/a unix socket for the dev backend) and
//! serves the protocol in `cove-proto`: one request per connection.

mod procs;
mod system;

use cove_proto::{line, Frame, Input, Request};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

trait Stream: Read + Write + Send {
    fn duplicate(&self) -> std::io::Result<Box<dyn Stream>>;
    fn close(&self);
}

impl Stream for std::net::TcpStream {
    fn duplicate(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn close(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

impl Stream for std::os::unix::net::UnixStream {
    fn duplicate(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn close(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(target_os = "linux")]
impl Stream for vsock::VsockStream {
    fn duplicate(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn close(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: cove-guest [--listen vsock:PORT | tcp:HOST:PORT | unix:PATH]\n\
         default: vsock:{}",
        cove_proto::GUEST_PORT
    );
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut listen = format!("vsock:{}", cove_proto::GUEST_PORT);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().unwrap_or_else(|| usage()),
            "--version" => {
                println!("cove-guest {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => usage(),
        }
    }
    // A client vanishing mid-write must not take the agent down.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    if let Err(e) = serve(&listen) {
        eprintln!("cove-guest: {e}");
        std::process::exit(1);
    }
}

fn serve(listen: &str) -> std::io::Result<()> {
    let (kind, rest) = listen.split_once(':').unwrap_or_else(|| usage());
    match kind {
        "tcp" => {
            let listener = std::net::TcpListener::bind(rest)?;
            eprintln!("cove-guest listening on tcp:{}", listener.local_addr()?);
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || handle(Box::new(conn)));
            }
        }
        "unix" => {
            let _ = std::fs::remove_file(rest);
            let listener = std::os::unix::net::UnixListener::bind(rest)?;
            eprintln!("cove-guest listening on unix:{rest}");
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || handle(Box::new(conn)));
            }
        }
        #[cfg(target_os = "linux")]
        "vsock" => {
            let port: u32 = rest.parse().unwrap_or_else(|_| usage());
            let listener =
                vsock::VsockListener::bind(&vsock::VsockAddr::new(vsock::VMADDR_CID_ANY, port))?;
            eprintln!("cove-guest listening on vsock:{port}");
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || handle(Box::new(conn)));
            }
        }
        _ => usage(),
    }
    Ok(())
}

type Writer = Arc<Mutex<Box<dyn Stream>>>;

fn send(writer: &Writer, frame: &Frame) -> bool {
    let mut w = writer.lock().unwrap();
    w.write_all(&line(frame)).and_then(|_| w.flush()).is_ok()
}

fn handle(conn: Box<dyn Stream>) {
    let Ok(read_half) = conn.duplicate() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let writer: Writer = Arc::new(Mutex::new(conn));
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let request: Request = match serde_json::from_str(first.trim()) {
        Ok(r) => r,
        Err(e) => {
            send(&writer, &Frame::error(format!("bad request: {e}")));
            return;
        }
    };
    match request {
        Request::Ping => {
            send(&writer, &Frame::ok(system::ping()));
        }
        Request::Exec(command) => match procs::Proc::exec(&command) {
            Ok(proc) => {
                let open = forward_input(reader, proc.clone());
                proc.follow(0, &|| open.load(Ordering::Relaxed), &mut |f| {
                    send(&writer, &f)
                });
                proc.release();
                if proc.running() {
                    // The client left before the command finished.
                    proc.signal(libc::SIGKILL);
                }
            }
            Err(e) => {
                send(&writer, &Frame::error(e));
            }
        },
        Request::Spawn { name, command } => match procs::spawn(&name, &command) {
            Ok(proc) => {
                send(&writer, &Frame::ok(proc.info()));
                let open = forward_input(reader, proc.clone());
                proc.follow(0, &|| open.load(Ordering::Relaxed), &mut |f| {
                    send(&writer, &f)
                });
            }
            Err(e) => {
                send(&writer, &Frame::error(e));
            }
        },
        Request::Attach { name, from } => match procs::get(&name) {
            Some(proc) => {
                send(&writer, &Frame::ok(proc.info()));
                let open = forward_input(reader, proc.clone());
                proc.follow(from, &|| open.load(Ordering::Relaxed), &mut |f| {
                    send(&writer, &f)
                });
            }
            None => {
                send(&writer, &Frame::error(format!("no process named {name:?}")));
            }
        },
        Request::Procs => {
            send(&writer, &Frame::ok(procs::list()));
        }
        Request::Kill { name, signal } => {
            if procs::kill(&name, signal.unwrap_or(libc::SIGTERM)) {
                send(&writer, &Frame::ok(true));
            } else {
                send(&writer, &Frame::error(format!("no process named {name:?}")));
            }
        }
        Request::ReadFile { path } => {
            send(&writer, &result(system::read_file(&path)));
        }
        Request::WriteFile { path, data, mode } => {
            send(&writer, &result(system::write_file(&path, &data.0, mode)));
        }
        Request::ListDir { path } => {
            send(&writer, &result(system::list_dir(&path)));
        }
        Request::Ports => {
            send(&writer, &Frame::ok(system::listening_ports()));
        }
        Request::Mount {
            tag,
            path,
            read_only,
        } => {
            send(&writer, &result(system::mount(&tag, &path, read_only)));
        }
        Request::Shutdown => {
            send(&writer, &Frame::ok(true));
            system::power_off();
        }
        Request::Tcp { port } => {
            let target = std::net::TcpStream::connect(("127.0.0.1", port))
                .or_else(|_| std::net::TcpStream::connect(("::1", port)));
            match target {
                Ok(target) => {
                    if send(&writer, &Frame::ok(true)) {
                        // Anything already buffered after the request line belongs to the pipe.
                        let buffered = reader.buffer().to_vec();
                        pipe(reader.into_inner(), writer, target, buffered);
                    }
                }
                Err(e) => {
                    send(
                        &writer,
                        &Frame::error(format!("nothing is listening on port {port}: {e}")),
                    );
                }
            }
        }
    }
}

fn result<T: serde::Serialize>(r: Result<T, String>) -> Frame {
    match r {
        Ok(v) => Frame::ok(v),
        Err(e) => Frame::error(e),
    }
}

/// Feed `Input` lines from the client to the process until the stream closes.
/// The returned flag turns false when it does.
fn forward_input(
    mut reader: BufReader<Box<dyn Stream>>,
    proc: Arc<procs::Proc>,
) -> Arc<AtomicBool> {
    let open = Arc::new(AtomicBool::new(true));
    let flag = open.clone();
    std::thread::spawn(move || {
        let mut text = String::new();
        loop {
            text.clear();
            match reader.read_line(&mut text) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(input) = serde_json::from_str::<Input>(text.trim()) {
                        proc.input(input);
                    }
                }
            }
        }
        flag.store(false, Ordering::Relaxed);
    });
    open
}

fn pipe(
    client_read: Box<dyn Stream>,
    client_write: Writer,
    target: std::net::TcpStream,
    buffered: Vec<u8>,
) {
    let Ok(mut target_write) = target.try_clone() else {
        return;
    };
    let mut client_read = client_read;
    let upstream = std::thread::spawn(move || {
        if !buffered.is_empty() && target_write.write_all(&buffered).is_err() {
            return;
        }
        let _ = std::io::copy(&mut client_read, &mut target_write);
        let _ = target_write.shutdown(std::net::Shutdown::Write);
    });
    let mut target_read = target;
    let mut buf = [0u8; 32 * 1024];
    loop {
        match target_read.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut w = client_write.lock().unwrap();
                if w.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    client_write.lock().unwrap().close();
    let _ = upstream.join();
}
