//! The single stdout writer. JSON-RPC needs every byte of the response
//! channel serialized; one thread owns stdout and the rest of the server
//! talks to it through `Sender<String>` (pre-rendered JSON lines).

use std::io::Write;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

/// Spawn the writer: takes the stdout lock, drains the channel until the
/// LAST sender drops. Broken pipe (client died) = quiet exit of the
/// writer; the main thread notices via join and stops serving.
pub(crate) fn spawn_writer(
    out: std::io::Stdout,
    rx: Receiver<String>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut out = std::io::BufWriter::new(out);
        for line in rx {
            if writeln!(out, "{line}").is_err() {
                return; // client gone: silent writer exit, no panic
            }
            if out.flush().is_err() {
                return;
            }
        }
    })
}