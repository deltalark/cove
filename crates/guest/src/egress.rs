//! The guest end of the egress proxy.
//!
//! Programs in the VM reach the proxy at `127.0.0.1:EGRESS_PORT`. The VM may have no
//! network device at all, so the guest can't dial out to the host. Instead the host
//! keeps a few `EgressAccept` streams open; each connection accepted here is handed
//! to one of them and piped through it.

use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

#[derive(Default)]
struct Queue {
    conns: Mutex<VecDeque<TcpStream>>,
    ready: Condvar,
}

/// One queue per port, each fed by its own listener thread.
fn queues() -> &'static Mutex<HashMap<u16, Arc<Queue>>> {
    static QUEUES: OnceLock<Mutex<HashMap<u16, Arc<Queue>>>> = OnceLock::new();
    QUEUES.get_or_init(Default::default)
}

fn queue(port: u16) -> Result<Arc<Queue>, String> {
    let mut queues = queues().lock().unwrap();
    if let Some(q) = queues.get(&port) {
        return Ok(q.clone());
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("could not listen on 127.0.0.1:{port}: {e}"))?;
    let q = listen(listener);
    queues.insert(port, q.clone());
    Ok(q)
}

fn listen(listener: TcpListener) -> Arc<Queue> {
    let q = Arc::new(Queue::default());
    let feed = q.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = conn.set_nodelay(true);
            feed.conns.lock().unwrap().push_back(conn);
            feed.ready.notify_one();
        }
    });
    q
}

/// Wait for the next program to connect to `port`.
pub fn accept(port: u16) -> Result<TcpStream, String> {
    let q = queue(port)?;
    let mut conns = q.conns.lock().unwrap();
    loop {
        if let Some(conn) = conns.pop_front() {
            return Ok(conn);
        }
        conns = q.ready.wait(conns).unwrap();
    }
}

/// Give a connection back when the host stream it was meant for has gone away.
pub fn requeue(port: u16, conn: TcpStream) {
    if let Ok(q) = queue(port) {
        q.conns.lock().unwrap().push_front(conn);
        q.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn connections_queue_until_taken() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        queues().lock().unwrap().insert(port, listen(listener));
        let mut a = TcpStream::connect(("127.0.0.1", port)).unwrap();
        a.write_all(b"one").unwrap();
        let mut got = accept(port).unwrap();
        let mut buf = [0u8; 3];
        got.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"one");
        // A connection handed back goes to the next taker first.
        let b = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let second = accept(port).unwrap();
        requeue(port, got);
        let again = accept(port).unwrap();
        assert_eq!(again.peer_addr().unwrap(), a.local_addr().unwrap());
        drop((b, second));
    }
}
