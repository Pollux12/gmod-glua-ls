//! A stderr sink that drops lines rather than blocking the server.
//!
//! An editor that starts the server as a child process reads its stderr and
//! shows it, which is what puts the log in front of a user. A client that does
//! not read it leaves the pipe to fill, and a full pipe blocks the *writer* —
//! which would be whichever analysis thread happened to log. A startup on a
//! large workspace writes well over a pipe buffer's worth, so that is not a
//! theoretical risk.
//!
//! Logging is diagnostics. It is never worth stalling analysis for, so lines
//! are handed to a background thread through a bounded queue and dropped when
//! that queue is full. Writing to the log file is unaffected.

use std::io::{self, Write};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

/// Lines allowed to queue before new ones are dropped. Enough to absorb the
/// bursts a startup produces while a reader is briefly behind.
const QUEUE_CAPACITY: usize = 4096;

pub struct NonBlockingStderr {
    sender: SyncSender<Vec<u8>>,
}

impl NonBlockingStderr {
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE_CAPACITY);

        // Detached: it ends when the sender is dropped, which happens when the
        // logger goes away, which happens when the process does.
        std::thread::Builder::new()
            .name("gluals-stderr".to_string())
            .spawn(move || {
                let stderr = io::stderr();
                for line in receiver {
                    let mut handle = stderr.lock();
                    // Nothing to do about a failed write to stderr except stop
                    // trying to report it.
                    let _ = handle.write_all(&line);
                    let _ = handle.flush();
                }
            })
            .ok();

        Self { sender }
    }
}

impl Write for NonBlockingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.sender.try_send(buf.to_vec()) {
            // A dropped line is the intended outcome when the reader is not
            // keeping up, so the caller is told the write succeeded.
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_report_success_and_never_block() {
        let mut sink = NonBlockingStderr::new();
        // Far more than the queue holds. If a full queue blocked or errored,
        // this would hang or fail rather than run to completion.
        for _ in 0..(QUEUE_CAPACITY * 2) {
            let written = sink.write(b"line\n").expect("write should not fail");
            assert_eq!(written, 5);
        }
        sink.flush().expect("flush should not fail");
    }
}
