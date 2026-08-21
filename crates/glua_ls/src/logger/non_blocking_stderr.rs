//! A stderr sink that drops whole log records rather than blocking the server.
//!
//! A client that does not read the server's stderr lets the pipe fill, and a
//! full pipe blocks the writer, which here is whichever analysis thread logged.
//! A startup writes more than a pipe buffer holds, so records go to a
//! background thread through a bounded queue and are dropped when it is full.
//!
//! `fern` splits one record over several `write` calls, so fragments are
//! accumulated here and queued only once a newline arrives. That keeps the queue
//! a count of lines rather than of format pieces, so a full queue drops whole
//! lines instead of cutting one in half. A record whose own message spans
//! several lines still queues one entry per line, and can lose some of them.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

/// Records allowed to queue before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;

pub struct NonBlockingStderr {
    /// `None` once the background thread is known to be gone.
    sender: Option<SyncSender<Vec<u8>>>,
    /// The fragments of the record being written, up to its newline.
    record: Vec<u8>,
    dropped: Arc<AtomicUsize>,
}

impl NonBlockingStderr {
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicUsize::new(0));

        let drain_dropped = dropped.clone();
        let sender = match std::thread::Builder::new()
            .name("gluals-stderr".to_string())
            .spawn(move || drain(receiver, drain_dropped))
        {
            Ok(_handle) => Some(sender),
            Err(error) => {
                // The logger is not up yet, so this is the only way to say it.
                eprintln!(
                    "gluals: could not start the stderr log thread ({error}); stderr logging is disabled"
                );
                None
            }
        };

        Self {
            sender,
            record: Vec::new(),
            dropped,
        }
    }

    #[cfg(test)]
    fn with_capacity(capacity: usize) -> (Self, Receiver<Vec<u8>>) {
        let (sender, receiver) = sync_channel::<Vec<u8>>(capacity);
        (
            Self {
                sender: Some(sender),
                record: Vec::new(),
                dropped: Arc::new(AtomicUsize::new(0)),
            },
            receiver,
        )
    }

    fn queue(&mut self, record: Vec<u8>) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };

        match sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => self.sender = None,
        }
    }
}

/// Writes queued records, prefixing however many were dropped while the queue
/// was full so the loss is visible in the output it interrupted.
fn drain(receiver: Receiver<Vec<u8>>, dropped: Arc<AtomicUsize>) {
    let stderr = io::stderr();
    for record in receiver {
        let missing = dropped.swap(0, Ordering::Relaxed);
        let mut handle = stderr.lock();
        if missing != 0 {
            let _ = writeln!(
                handle,
                "gluals: dropped {missing} log record(s); stderr is not being read fast enough"
            );
        }
        let _ = handle.write_all(&record);
        let _ = handle.flush();
    }
}

impl Write for NonBlockingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.record.extend_from_slice(buf);
        while let Some(end) = self.record.iter().position(|byte| *byte == b'\n') {
            let record = self.record.drain(..=end).collect();
            self.queue(record);
        }

        // A dropped record is the intended outcome, so the write reports success.
        Ok(buf.len())
    }

    /// Queues whatever has been written without a terminating newline.
    ///
    /// It cannot wait for the queue to drain: `fern` flushes after every
    /// record, so a flush that blocked until the background thread caught up
    /// would put the calling thread back behind the stderr pipe, which is the
    /// stall this sink exists to avoid.
    fn flush(&mut self) -> io::Result<()> {
        if !self.record.is_empty() {
            let record = std::mem::take(&mut self.record);
            self.queue(record);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_report_success_and_never_block() {
        let (mut sink, _receiver) = NonBlockingStderr::with_capacity(4);
        // Far more than the queue holds. If a full queue blocked or errored,
        // this would hang or fail rather than run to completion.
        for _ in 0..(QUEUE_CAPACITY * 2) {
            let written = sink.write(b"line\n").expect("write should not fail");
            assert_eq!(written, 5);
        }
        sink.flush().expect("flush should not fail");
    }

    #[test]
    fn a_record_queues_once_however_many_writes_it_takes() {
        let (mut sink, receiver) = NonBlockingStderr::with_capacity(4);

        // Exactly the shape fern writes a record with; each format piece
        // reaches `write` on its own.
        let line_sep = "\n";
        write!(
            sink,
            "{}{}",
            format_args!("[{}] {}", "INFO", "hello"),
            line_sep
        )
        .expect("write should not fail");
        sink.flush().expect("flush should not fail");

        assert_eq!(receiver.try_recv().expect("one record"), b"[INFO] hello\n");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_full_queue_drops_whole_records_and_counts_them() {
        let (mut sink, receiver) = NonBlockingStderr::with_capacity(1);

        sink.write_all(b"first\n").expect("write should not fail");
        sink.write_all(b"second\n").expect("write should not fail");
        sink.write_all(b"third\n").expect("write should not fail");

        assert_eq!(receiver.try_recv().expect("one record"), b"first\n");
        assert!(receiver.try_recv().is_err());
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 2);
    }
}
