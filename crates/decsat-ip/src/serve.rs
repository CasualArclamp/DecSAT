// made by CasualArclamp
//! A small streaming server for media players: TS, or an audio elementary
//! stream.
//!
//! Each connected client gets the live stream. A client that speaks HTTP
//! (`GET …`) gets an HTTP response first, so VLC and PotPlayer can open
//! `http://host:port/`; one that says nothing (VLC's `tcp://host:port`) gets
//! the raw bytes. Every client has its own thread and a bounded queue, so a
//! slow or stalled player loses data rather than holding up the receiver.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Chunks a client may have queued before new ones are dropped for it.
const CLIENT_QUEUE: usize = 256;

/// A stream's own start: a file header every client needs, however late it
/// joins, and the content type to say it is.
#[derive(Clone)]
struct Prelude {
    content_type: &'static str,
    bytes: Vec<u8>,
}

struct Client {
    peer: SocketAddr,
    tx: SyncSender<Arc<Vec<u8>>>,
    alive: Arc<AtomicBool>,
}

/// A TCP (and HTTP) server streaming to every connected client.
pub struct StreamServer {
    pub addr: SocketAddr,
    /// What a new client is sent first (and its HTTP content type), when
    /// the stream is not the plain one: [`StreamServer::set_prelude`].
    prelude: Arc<Mutex<Option<Prelude>>>,
    clients: Arc<Mutex<Vec<Client>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    pub dropped_chunks: u64,
}

impl StreamServer {
    /// Listen on `addr` (port 0 picks a free one; see `addr` after),
    /// answering HTTP clients with `content_type`.
    pub fn bind(addr: SocketAddr, content_type: &'static str) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let prelude: Arc<Mutex<Option<Prelude>>> = Arc::new(Mutex::new(None));
        let accept = {
            let (clients, stop, prelude) = (clients.clone(), stop.clone(), prelude.clone());
            std::thread::Builder::new()
                .name("decsat-serve".into())
                .spawn(move || accept_loop(listener, clients, stop, content_type, prelude))?
        };
        Ok(StreamServer {
            addr,
            prelude,
            clients,
            stop,
            accept: Some(accept),
            dropped_chunks: 0,
        })
    }

    /// From now on every new client is told `content_type` and sent `bytes`
    /// before the stream (a container's header, for a stream that is not
    /// plain TS).
    pub fn set_prelude(&mut self, content_type: &'static str, bytes: Vec<u8>) {
        *self.prelude.lock().unwrap() = Some(Prelude {
            content_type,
            bytes,
        });
    }

    /// Addresses of the players connected now.
    pub fn clients(&self) -> Vec<SocketAddr> {
        let mut c = self.clients.lock().unwrap();
        c.retain(|c| c.alive.load(Ordering::Relaxed));
        c.iter().map(|c| c.peer).collect()
    }

    /// Send `chunk` to every client.
    pub fn write(&mut self, chunk: Vec<u8>) {
        if chunk.is_empty() {
            return;
        }
        let mut c = self.clients.lock().unwrap();
        if c.is_empty() {
            return;
        }
        let chunk = Arc::new(chunk);
        c.retain(|cl| {
            if !cl.alive.load(Ordering::Relaxed) {
                return false;
            }
            match cl.tx.try_send(chunk.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    self.dropped_chunks += 1;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Dropping the senders ends the client threads.
        self.clients.lock().unwrap().clear();
        if let Some(j) = self.accept.take() {
            let _ = j.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    clients: Arc<Mutex<Vec<Client>>>,
    stop: Arc<AtomicBool>,
    content_type: &'static str,
    prelude: Arc<Mutex<Option<Prelude>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let (tx, rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE);
                let alive = Arc::new(AtomicBool::new(true));
                let a = alive.clone();
                let prelude = prelude.clone();
                let spawned = std::thread::Builder::new()
                    .name("decsat-serve-client".into())
                    .spawn(move || {
                        serve_client(stream, rx, content_type, prelude);
                        a.store(false, Ordering::Relaxed);
                    });
                if spawned.is_ok() {
                    clients.lock().unwrap().push(Client { peer, tx, alive });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Answer an HTTP request if the client sends one, then stream.
fn serve_client(
    mut s: TcpStream,
    rx: mpsc::Receiver<Arc<Vec<u8>>>,
    content_type: &str,
    prelude: Arc<Mutex<Option<Prelude>>>,
) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_nodelay(true);
    // A player speaking HTTP sends its request at once; a raw TCP client
    // sends nothing. Wait briefly to tell which.
    let _ = s.set_read_timeout(Some(Duration::from_millis(400)));
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    while req.len() < 8192 {
        match s.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break, // timeout: a raw client
        }
    }
    // The stream's kind must be known before the HTTP head (and the header
    // of a container) goes out: the first chunk only comes once it is (a
    // prelude is set before the first of its chunks).
    let Ok(first) = rx.recv() else { return };
    let first = Some(first);
    let pre = prelude.lock().unwrap().clone();
    let content_type = pre.as_ref().map_or(content_type, |p| p.content_type);
    if req.starts_with(b"GET ") || req.starts_with(b"HEAD ") {
        let head = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\n\
             Connection: close\r\nServer: DecSAT\r\n\r\n"
        );
        if s.write_all(head.as_bytes()).is_err() || req.starts_with(b"HEAD ") {
            return;
        }
    }
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    if let Some(p) = &pre
        && s.write_all(&p.bytes).is_err()
    {
        return;
    }
    if let Some(chunk) = first
        && s.write_all(&chunk).is_err()
    {
        return;
    }
    while let Ok(chunk) = rx.recv() {
        if s.write_all(&chunk).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_raw_and_http_clients() {
        let mut srv = StreamServer::bind("127.0.0.1:0".parse().unwrap(), "audio/aac").unwrap();
        let addr = srv.addr;
        let mut raw = TcpStream::connect(addr).unwrap();
        let mut http = TcpStream::connect(addr).unwrap();
        http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let t0 = std::time::Instant::now();
        while srv.clients().len() < 2 && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(600));
        srv.write(b"hello".to_vec());

        raw.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut got = [0u8; 5];
        raw.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");

        http.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            http.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.0 200 OK"));
        assert!(head.contains("audio/aac"));
        http.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
    }
}
