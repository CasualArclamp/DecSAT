// made by CasualArclamp
//! Live IP into Wireshark. Started with `-k -i -`, Wireshark captures from
//! its standard input (through dumpcap), which this feeds as a PCAP stream:
//! raw IP, as the PCAP files are. The writing has a thread of its own
//! behind a bounded queue, so a Wireshark that falls behind costs packets
//! (counted), never the decoder's time.
//!
//! On a DecSAT server (smart mode) the decoder's thread carries a
//! [`PacketRoute`] ([`set_route`]): Wireshark is then not started there, and
//! the packets go to the client, which starts its own.

use std::io::BufWriter;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::time::SystemTime;

use decsat_ip::PcapWriter;

/// Packets waiting for Wireshark at most; more are dropped.
const QUEUE: usize = 4096;

type Packet = (SystemTime, Vec<u8>);

/// Where a server's decoders send the packets they would show in Wireshark.
pub trait PacketSink: Send + Sync {
    /// One packet for feed `stream`, whose window is titled `title`.
    fn packet(&self, stream: u64, title: &str, at: SystemTime, packet: &[u8]);
    /// Feed `stream` has stopped.
    fn end(&self, stream: u64);
}

/// A cheap handle on a [`PacketSink`].
pub type PacketRoute = Arc<dyn PacketSink>;

thread_local! {
    // Rust note: one value per thread (see `decsat_audio::route`).
    static ROUTE: std::cell::RefCell<Option<PacketRoute>> = const { std::cell::RefCell::new(None) };
}

/// Feeds started on this thread from now on go along `route` rather than
/// to a Wireshark here (`None`: here, as normal).
pub fn set_route(route: Option<PacketRoute>) {
    ROUTE.with(|r| *r.borrow_mut() = route);
}

/// This thread's route, for a thread it spawns to take up.
pub fn route() -> Option<PacketRoute> {
    ROUTE.with(|r| r.borrow().clone())
}

/// A feed sent along a route: the sink, the feed's number and its title.
struct Remote {
    sink: PacketRoute,
    stream: u64,
    title: String,
}

/// A Wireshark capturing what is sent to it. Dropping this closes its
/// input: its capture stops, and the packets it has stay on its screen.
pub struct Live {
    /// `None` once the writer has gone (Wireshark closed, or its capture
    /// stopped).
    tx: Option<SyncSender<Packet>>,
    /// `None` for a feed sent along a route.
    child: Option<Child>,
    remote: Option<Remote>,
    /// Packets written to Wireshark, counted by the writer thread (an
    /// `Arc`'d atomic: shared between the threads without a lock).
    written: Arc<AtomicU64>,
    /// Packets dropped because the queue was full.
    pub dropped: u64,
}

impl Live {
    /// Start Wireshark capturing from this, `title` added to its window's.
    /// Its RTP-over-UDP heuristic is turned on: ED-137's streams (with
    /// their PTT and squelch) are dissected without the SIP that set them
    /// up, which a capture started mid-session never sees. Ports with a
    /// dissector of their own still go to it first.
    pub fn start(title: &str) -> Result<Self, String> {
        if let Some(sink) = route() {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            return Ok(Live {
                tx: None,
                child: None,
                remote: Some(Remote {
                    sink,
                    stream: NEXT.fetch_add(1, Ordering::Relaxed),
                    title: title.to_string(),
                }),
                written: Arc::new(AtomicU64::new(0)),
                dropped: 0,
            });
        }
        let exe = find();
        let mut child = Command::new(&exe)
            .args(["-k", "-i", "-", "--enable-heuristic", "rtp_udp", "-o"])
            .arg(format!("gui.window_title:{title}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "Wireshark not found (wireshark.org): record a PCAP instead".to_string()
                }
                _ => format!("{}: {e}", exe.display()),
            })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let (tx, rx) = sync_channel(QUEUE);
        let written = Arc::new(AtomicU64::new(0));
        let count = written.clone();
        std::thread::Builder::new()
            .name("wireshark".into())
            // `move`: the thread takes ownership of the pipe, the queue's
            // receiving end and its counter.
            .spawn(move || feed(stdin, &rx, &count))
            .map_err(|e| e.to_string())?;
        Ok(Live {
            tx: Some(tx),
            child: Some(child),
            remote: None,
            written,
            dropped: 0,
        })
    }

    /// Queue one packet; false once Wireshark has gone.
    pub fn send(&mut self, at: SystemTime, packet: &[u8]) -> bool {
        if let Some(r) = &self.remote {
            r.sink.packet(r.stream, &r.title, at, packet);
            self.written.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let Some(tx) = &self.tx else {
            return false;
        };
        match tx.try_send((at, packet.to_vec())) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                true
            }
            Err(TrySendError::Disconnected(_)) => {
                self.tx = None;
                false
            }
        }
    }

    /// Wireshark is still there and taking packets: its window is open
    /// and nothing has failed to reach it.
    pub fn alive(&mut self) -> bool {
        if self.remote.is_some() {
            return true;
        }
        self.tx.is_some()
            && self
                .child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(r) = &self.remote {
            r.sink.end(r.stream);
        }
    }
}

/// The writer thread: the PCAP header, then each packet as it comes,
/// flushed whenever the queue runs dry so packets show at once. It ends
/// when the queue's sender is dropped or a write fails (Wireshark gone);
/// either way the pipe closes as `w` is dropped.
fn feed(stdin: ChildStdin, rx: &Receiver<Packet>, written: &AtomicU64) {
    let Ok(mut w) = PcapWriter::new(BufWriter::new(stdin)) else {
        return;
    };
    loop {
        let (at, p) = match rx.try_recv() {
            Ok(p) => p,
            Err(TryRecvError::Empty) => {
                if w.flush().is_err() {
                    return;
                }
                match rx.recv() {
                    Ok(p) => p,
                    Err(_) => return,
                }
            }
            Err(TryRecvError::Disconnected) => return,
        };
        if w.write(at, &p).is_err() {
            return;
        }
        written.fetch_add(1, Ordering::Relaxed);
    }
}

/// Where Wireshark's installer puts it, else its name for the PATH.
fn find() -> PathBuf {
    let installed = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join("Wireshark").join("Wireshark.exe"))
        .chain([PathBuf::from(
            "/Applications/Wireshark.app/Contents/MacOS/Wireshark",
        )])
        .find(|p| p.exists());
    installed.unwrap_or_else(|| PathBuf::from("wireshark"))
}
