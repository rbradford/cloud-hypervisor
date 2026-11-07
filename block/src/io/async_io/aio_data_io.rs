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
        self.submit_request(InFlightRequest::Data(op))
    }

    /// Submits an fsync operation carrying `user_data`.
    pub fn submit_fsync(&mut self, user_data: u64) -> io::Result<()> {
        self.submit_request(InFlightRequest::Fsync { user_data })
    }

    fn submit_request(&mut self, request: InFlightRequest) -> io::Result<()> {
        let user_data = request.user_data();
        let mut iocb = Self::build_iocb(&self.file, self.completions.notifier(), &request);
        self.in_flight.insert(request)?;

        let result = match self.ctx.submit(&[&mut iocb]) {
            Ok(1) => return Ok(()),
            Ok(_) => -libc::EAGAIN,
            Err(e) => errno_result(&e),
        };

        if let Some(completion) = self.complete_request(user_data, result) {
            self.inject_completion(completion);
        }
        Ok(())
    }

    fn complete_request(&mut self, user_data: u64, result: i32) -> Option<AsyncIoCompletion> {
        let ctx = &self.ctx;
        let file = &self.file;
        let notifier = self.completions.notifier();
        self.in_flight.complete(user_data, result, |request| {
            let mut iocb = Self::build_iocb(file, notifier, request);
            match ctx.submit(&[&mut iocb]) {
                Ok(1) => Ok(()),
                Ok(_) => Err(io::Error::from_raw_os_error(libc::EAGAIN)),
                Err(error) => Err(error),
            }
        })
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

        self.reap_completions();
        self.completions.next_completed()
    }

    fn reap_completions(&mut self) {
        let mut events = [aio::IoEvent::default(); 32];
        let rc = match self.ctx.get_events(0, &mut events, None) {
            Ok(rc) => rc,
            Err(e) => {
                warn!("Linux AIO get_events failed: {e}");
                return;
            }
        };
        for event in &events[..rc] {
            if let Some(completion) = self.complete_request(event.data, event.res as i32) {
                self.completions.complete(completion);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::thread::sleep;
    use std::time::Duration;

    use vmm_sys_util::tempfile::TempFile;

    use super::{AioDataIo, InFlightRequest};
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

    #[test]
    fn retries_retained_data_and_fsync_requests() {
        use std::os::unix::fs::FileExt;

        let file = TempFile::new().unwrap().into_file();
        let mut data_io = AioDataIo::new(file, 8).unwrap();
        for error in [libc::EAGAIN, libc::EINTR] {
            data_io.file.write_all_at(&[0xa5; 512], 512).unwrap();
            data_io
                .in_flight
                .insert(InFlightRequest::Data(AsyncIoOperation::read_to_vec(
                    512,
                    OwnedIoBuffer::from_vec(vec![0; 512]),
                    7,
                )))
                .unwrap();
            // Simulate a rejected initial attempt, with no kernel I/O outstanding.
            assert!(data_io.complete_request(7, -error).is_none());
            assert_eq!(
                data_io.submit_fsync(7).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            let completion = wait_for_completion(&mut data_io);
            assert_eq!(completion.user_data, 7);
            assert_eq!(completion.result, 512);
            assert_eq!(completion.buffer.unwrap().as_slice(), &[0xa5; 512]);

            data_io
                .in_flight
                .insert(InFlightRequest::Data(AsyncIoOperation::write_from_vec(
                    512,
                    OwnedIoBuffer::from_vec(vec![0x5a; 512]),
                    8,
                )))
                .unwrap();
            assert!(data_io.complete_request(8, -error).is_none());
            let completion = wait_for_completion(&mut data_io);
            assert_eq!(completion.user_data, 8);
            assert_eq!(completion.result, 512);
            let mut buffer = [0; 512];
            data_io.file.read_exact_at(&mut buffer, 512).unwrap();
            assert_eq!(buffer, [0x5a; 512]);

            data_io
                .in_flight
                .insert(InFlightRequest::Fsync { user_data: 9 })
                .unwrap();
            assert!(data_io.complete_request(9, -error).is_none());
            let completion = wait_for_completion(&mut data_io);
            assert_eq!(completion.user_data, 9);
            assert_eq!(completion.result, 0);
            assert!(data_io.next_completion().is_none());
        }
    }

    #[test]
    fn repeated_kernel_eagain_completes_with_error() {
        use std::fs::File;
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;

        // An empty nonblocking socket produces a real -EAGAIN completion.
        let (reader, _writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut data_io = AioDataIo::new(File::from(OwnedFd::from(reader)), 8).unwrap();
        data_io
            .submit_operation(AsyncIoOperation::read_to_vec(
                0,
                OwnedIoBuffer::from_vec(vec![0; 8]),
                7,
            ))
            .unwrap();
        let completion = wait_for_completion(&mut data_io);
        assert_eq!(completion.user_data, 7);
        assert_eq!(completion.result, -libc::EAGAIN);
        assert!(completion.buffer.is_some());
        assert!(data_io.next_completion().is_none());
    }

    #[test]
    fn drop_retains_guest_memory_until_retried_io_finishes() {
        use std::sync::Arc;

        use vm_memory::{GuestAddress, GuestMemoryMmap};

        use crate::async_io::GuestMemoryTarget;

        let memory =
            Arc::new(GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 4096)]).unwrap());
        let target =
            GuestMemoryTarget::new(Arc::clone(&memory), &[(GuestAddress(0), 512)]).unwrap();
        let file = TempFile::new().unwrap().into_file();
        file.set_len(512).unwrap();
        let mut data_io = AioDataIo::new(file, 8).unwrap();
        data_io
            .in_flight
            .insert(InFlightRequest::Data(AsyncIoOperation::read_to_memory(
                0, target, 7,
            )))
            .unwrap();
        assert!(data_io.complete_request(7, -libc::EINTR).is_none());
        assert_eq!(Arc::strong_count(&memory), 2);
        drop(data_io);
        assert_eq!(Arc::strong_count(&memory), 1);
    }
}
