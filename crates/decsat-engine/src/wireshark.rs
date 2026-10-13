//! Live IP into Wireshark. Started with `-k -i -`, Wireshark captures from
//! its standard input (through dumpcap), which this feeds as a PCAP stream:
//! raw IP, as the PCAP files are. The writing has a thread of its own
//! behind a bounded queue, so a Wireshark that falls behind costs packets
//! (counted), never the decoder's time.

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

/// A Wireshark capturing what is sent to it. Dropping this closes its
/// input: its capture stops, and the packets it has stay on its screen.
pub struct Live {
    /// `None` once the writer has gone (Wireshark closed, or its capture
    /// stopped).
    tx: Option<SyncSender<Packet>>,
    child: Child,
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
            child,
            written,
            dropped: 0,
        })
    }

    /// Queue one packet; false once Wireshark has gone.
    pub fn send(&mut self, at: SystemTime, packet: &[u8]) -> bool {
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
        self.tx.is_some() && matches!(self.child.try_wait(), Ok(None))
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
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
