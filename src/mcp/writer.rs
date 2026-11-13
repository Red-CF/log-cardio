//! The single stdout writer. JSON-RPC needs every byte of the response
//! channel serialized; one thread owns stdout and the rest of the server
//! talks to it through `Sender<String>` (pre-rendered JSON lines).

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

/// Spawn the writer: takes the stdout lock, drains the channel until the
/// LAST sender drops. Broken pipe (client died) = quiet exit of the
/// writer; the main thread notices via join and stops serving.
pub(crate) fn spawn_writer(
    out: std::io::Stdout,
    rx: Receiver<String>,
    client_gone: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut out = std::io::BufWriter::new(out);
        for line in rx {
            if writeln!(out, "{line}").is_err() {
                // client gone: announce ONCE so the rest of the server
                // stops paying for fetches nobody will ever read (the
                // 1.x "keep burning bq scans for a dead client" flaw)
                client_gone.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            if out.flush().is_err() {
                client_gone.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        }
    })
}