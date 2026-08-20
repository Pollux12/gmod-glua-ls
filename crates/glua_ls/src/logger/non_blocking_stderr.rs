//! A stderr sink that drops lines rather than blocking the server.
//!
//! A client that does not read the server's stderr lets the pipe fill, and a
//! full pipe blocks the writer, which here is whichever analysis thread logged.
//! A startup writes more than a pipe buffer holds, so lines go to a background
//! thread through a bounded queue and are dropped when it is full.

use std::io::{self, Write};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

/// Lines allowed to queue before new ones are dropped.
const QUEUE_CAPACITY: usize = 4096;

pub struct NonBlockingStderr {
    sender: SyncSender<Vec<u8>>,
}

impl NonBlockingStderr {
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(QUEUE_CAPACITY);

        std::thread::Builder::new()
            .name("gluals-stderr".to_string())
            .spawn(move || {
                let stderr = io::stderr();
                for line in receiver {
                    let mut handle = stderr.lock();
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
            // A dropped line is the intended outcome, so the write reports success.
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
