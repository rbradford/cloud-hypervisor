// Copyright © 2023 Intel Corporation
//
// Copyright © 2023 Crusoe Energy Systems LLC
//
// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use log::warn;
use vmm_sys_util::aio;
use vmm_sys_util::eventfd::EventFd;

use super::common::{InFlightRequest, InFlightRequests, errno_result};
use super::{AsyncIoCompletion, AsyncIoOperation, CompletionCommon};

/// Retained Linux AIO queue for owned async data I/O operations.
pub struct AioDataIo {
    // Keep this before `in_flight`: Rust drops fields in declaration order, so
    // dropping the context destroys kernel AIO state before retained
    // operations release the buffers referenced by their iovecs.
    ctx: aio::IoContext,
    // Keep the file open until the kernel AIO context has been destroyed.
    file: File,
    in_flight: InFlightRequests,
    completions: CompletionCommon,
}

impl AioDataIo {
    /// Creates a Linux AIO context for `file` and its completion eventfd.
    pub fn new(file: File, queue_depth: u32) -> io::Result<Self> {
        Ok(Self {
            ctx: aio::IoContext::new(queue_depth)?,
            file,
            in_flight: InFlightRequests::default(),
            completions: CompletionCommon::new(),
        })
    }

    /// Returns the eventfd signaled when completions are available.
    pub fn notifier(&self) -> &EventFd {
        self.completions.notifier()
    }

    /// Submits one owned read or write operation to the queue.
    ///
    /// Submission failures are converted into injected completions so callers
    /// can observe every accepted request through the normal completion path.
    pub fn submit_operation(&mut self, op: AsyncIoOperation) -> io::Result<()> {
        let request = InFlightRequest::Data(op);
        let user_data = request.user_data();
        let mut iocb = Self::build_iocb(&self.file, self.completions.notifier(), &request);
        self.in_flight.insert(request)?;

        let result = match self.ctx.submit(&[&mut iocb]) {
            Ok(1) => return Ok(()),
            Ok(_) => -libc::EAGAIN,
            Err(e) => errno_result(&e),
        };

        let buffer = self
            .in_flight
            .remove(user_data)
            .and_then(InFlightRequest::into_completion_buffer);
        self.inject_completion(AsyncIoCompletion::new(user_data, result, buffer));
        Ok(())
    }

    /// Submits an fsync operation carrying `user_data`.
    pub fn submit_fsync(&mut self, user_data: u64) -> io::Result<()> {
        let request = InFlightRequest::Fsync { user_data };
        let mut iocb = Self::build_iocb(&self.file, self.completions.notifier(), &request);
        self.in_flight.insert(request)?;

        let result = match self.ctx.submit(&[&mut iocb]) {
            Ok(1) => return Ok(()),
            Ok(_) => -libc::EAGAIN,
            Err(e) => errno_result(&e),
        };

        self.in_flight.remove(user_data);
        self.inject_completion(AsyncIoCompletion::new(user_data, result, None));
        Ok(())
    }

    fn build_iocb(
        file: &File,
        notifier: &EventFd,
        request: &InFlightRequest,
    ) -> aio::IoControlBlock {
        let mut iocb = aio::IoControlBlock {
            aio_fildes: file.as_raw_fd() as u32,
            aio_data: request.user_data(),
            aio_flags: aio::IOCB_FLAG_RESFD,
            aio_resfd: notifier.as_raw_fd() as u32,
            ..Default::default()
        };

        match request {
            InFlightRequest::Data(op) => {
                let iovecs = op.iovecs();
                iocb.aio_lio_opcode = if op.is_read() {
                    aio::IOCB_CMD_PREADV as u16
                } else {
                    aio::IOCB_CMD_PWRITEV as u16
                };
                iocb.aio_buf = iovecs.as_ptr() as u64;
                iocb.aio_nbytes = iovecs.len() as u64;
                iocb.aio_offset = op.offset();
            }
            InFlightRequest::Fsync { .. } => {
                iocb.aio_lio_opcode = aio::IOCB_CMD_FSYNC as u16;
            }
        }

        iocb
    }

    /// Injects a completion that did not come from a kernel AIO event.
    ///
    /// The notifier is signaled so callers can drain it with
    /// [`Self::next_completion`].
    pub fn inject_completion(&mut self, completion: AsyncIoCompletion) {
        self.completions.complete(completion);
    }

    /// Returns the next kernel or injected completion if one is available.
    ///
    /// Consuming a kernel completion returns ownership of any buffer retained
    /// by the corresponding operation.
    pub fn next_completion(&mut self) -> Option<AsyncIoCompletion> {
        if let Some(completion) = self.completions.next_completed() {
            return Some(completion);
        }

        let mut events = [aio::IoEvent::default(); 32];
        let rc = match self.ctx.get_events(0, &mut events, None) {
            Ok(rc) => rc,
            Err(e) => {
                warn!("Linux AIO get_events failed: {e}");
                return None;
            }
        };
        for event in &events[..rc] {
            self.completions.complete(AsyncIoCompletion::new(
                event.data,
                event.res as i32,
                self.in_flight
                    .remove(event.data)
                    .and_then(InFlightRequest::into_completion_buffer),
            ));
        }

        self.completions.next_completed()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::thread::sleep;
    use std::time::Duration;

    use vmm_sys_util::tempfile::TempFile;

    use super::AioDataIo;
    use crate::async_io::{AsyncIoCompletion, AsyncIoOperation, OwnedIoBuffer};

    fn wait_for_completion(data_io: &mut AioDataIo) -> AsyncIoCompletion {
        for _ in 0..1000 {
            if let Some(completion) = data_io.next_completion() {
                return completion;
            }
            sleep(Duration::from_millis(1));
        }

        panic!("timed out waiting for Linux AIO completion");
    }

    #[test]
    fn aio_rejects_duplicate_user_data_for_metadata_ops() {
        let mut file = TempFile::new().unwrap().into_file();
        file.write_all(&[0xa5; 512]).unwrap();
        let mut data_io = AioDataIo::new(file, 8).unwrap();

        data_io
            .submit_operation(AsyncIoOperation::read_to_vec(
                0,
                OwnedIoBuffer::from_vec(vec![0; 512]),
                7,
            ))
            .unwrap();

        assert_eq!(
            data_io.submit_fsync(7).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        let completion = wait_for_completion(&mut data_io);
        assert_eq!(completion.user_data, 7);
        assert_eq!(completion.result, 512);
        assert_eq!(
            completion.buffer.unwrap().as_slice(),
            [0xa5; 512].as_slice()
        );

        data_io.submit_fsync(7).unwrap();
        let completion = wait_for_completion(&mut data_io);
        assert_eq!(completion.user_data, 7);
        assert_eq!(completion.result, 0);
    }

    #[test]
    fn aio_injected_completion_uses_completion_path() {
        let file = TempFile::new().unwrap().into_file();
        let mut data_io = AioDataIo::new(file, 8).unwrap();

        data_io.inject_completion(AsyncIoCompletion::new(9, -libc::EIO, None));

        let completion = data_io.next_completion().unwrap();
        assert_eq!(completion.user_data, 9);
        assert_eq!(completion.result, -libc::EIO);
        assert!(completion.buffer.is_none());
        assert!(data_io.next_completion().is_none());
    }
}
