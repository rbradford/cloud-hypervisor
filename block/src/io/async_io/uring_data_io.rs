// Copyright © 2021 Intel Corporation
//
// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::fs::File;
use std::os::fd::AsRawFd;
use std::{io, mem};

use io_uring::register::Restriction;
use io_uring::{IoUring, opcode, squeue, types};
use log::{error, warn};
use vmm_sys_util::eventfd::EventFd;

use super::common::{InFlightRequest, InFlightRequests, errno_result};
use super::{AsyncIoCompletion, AsyncIoOperation, CompletionCommon};

// Linux UAPI constant for enabling a ring created with IORING_SETUP_R_DISABLED
const IORING_REGISTER_ENABLE_RINGS: u8 = 12;

/// `io_uring` wrapper for async I/O.
///
/// Holds the `IoUring` and tracks the ops that are pending.
pub struct UringDataIo {
    io_uring: IoUring,
    // Keep the file open through draining and closing the ring.
    file: File,
    in_flight: InFlightRequests,
    completions: CompletionCommon,
    // `needs_submit_retry` is set when SQEs have been published to the ring,
    // but the submit syscall failed before confirming kernel ownership.
    needs_submit_retry: bool,
}

impl UringDataIo {
    /// Creates an io_uring queue for `file` and registers its completion eventfd.
    pub fn new(file: File, ring_depth: u32) -> io::Result<Self> {
        let io_uring = IoUring::builder().setup_r_disabled().build(ring_depth)?;
        let completions = CompletionCommon::new();
        io_uring
            .submitter()
            .register_eventfd(completions.notifier().as_raw_fd())?;

        io_uring.submitter().register_restrictions(&mut [
            Restriction::register_op(IORING_REGISTER_ENABLE_RINGS),
            Restriction::sqe_op(opcode::Readv::CODE),
            Restriction::sqe_op(opcode::Writev::CODE),
            Restriction::sqe_op(opcode::Fsync::CODE),
            Restriction::sqe_flags_allowed(0),
        ])?;
        io_uring.submitter().register_enable_rings()?;

        Ok(Self {
            io_uring,
            file,
            in_flight: InFlightRequests::default(),
            completions,
            needs_submit_retry: false,
        })
    }

    /// Returns the eventfd signaled when completions are available.
    pub fn notifier(&self) -> &EventFd {
        self.completions.notifier()
    }

    /// Submits one owned read or write operation to the queue.
    pub fn submit_operation(&mut self, op: AsyncIoOperation) -> io::Result<()> {
        self.submit_batch(vec![op])
    }

    fn submit_kernel_entry(
        &mut self,
        request: InFlightRequest,
        entry: &squeue::Entry,
    ) -> io::Result<()> {
        let user_data = request.user_data();
        self.in_flight.insert(request)?;

        if let Err(error) = Self::submit_entry(
            &mut self.io_uring,
            &mut self.needs_submit_retry,
            self.completions.notifier(),
            entry,
        ) {
            self.reap_completions();
            if let Some(completion) = self.complete_request(user_data, errno_result(&error)) {
                self.inject_completion(completion);
            }
        }
        Ok(())
    }

    fn submit_entry(
        io_uring: &mut IoUring,
        needs_submit_retry: &mut bool,
        notifier: &EventFd,
        entry: &squeue::Entry,
    ) -> io::Result<()> {
        let (submitter, mut sq, _) = io_uring.split();
        if sq.is_full() {
            // Existing entries are already published. Submit them to free SQ slots.
            match submitter.submit() {
                Ok(_) => *needs_submit_retry = false,
                Err(error) => {
                    *needs_submit_retry = true;
                    warn!("io_uring submit failed for published SQEs: {error}");
                    notifier.write(1).unwrap();
                }
            }
            // Refresh the cached head before trying to push the new entry.
            sq.sync();
        }

        // SAFETY: the caller retains the request and its buffers in in_flight
        // before publishing this entry, and keeps them until its CQE is consumed.
        unsafe { sq.push(entry) }.map_err(|_| io::Error::from_raw_os_error(libc::EAGAIN))?;
        sq.sync();
        match submitter.submit() {
            Ok(_) => *needs_submit_retry = false,
            Err(error) => {
                *needs_submit_retry = true;
                warn!("io_uring submit failed after SQE was published: {error}");
                notifier.write(1).unwrap();
            }
        }
        // Once published, this entry must not be pushed a second time.
        Ok(())
    }

    fn complete_request(&mut self, user_data: u64, result: i32) -> Option<AsyncIoCompletion> {
        let file = &self.file;
        let io_uring = &mut self.io_uring;
        let needs_submit_retry = &mut self.needs_submit_retry;
        let notifier = self.completions.notifier();
        self.in_flight.complete(user_data, result, |request| {
            let entry = Self::build_entry(file, request);
            Self::submit_entry(io_uring, needs_submit_retry, notifier, &entry)
        })
    }

    /// Submits a batch of owned read and write operations.
    ///
    /// If the submission queue cannot accept the whole batch, reap completions
    /// and retry its operations individually. Retry exhaustion is reported
    /// through the normal completion path.
    pub fn submit_batch(&mut self, batch: Vec<AsyncIoOperation>) -> io::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        self.in_flight.validate_batch(&batch)?;

        let (submitter, mut sq, _) = self.io_uring.split();
        let available = sq.capacity() - sq.len();
        if batch.len() > available {
            // Not enough space for the batch.
            // Drop sq, which will re-publish an unmodified tail pointer
            drop(sq);
            self.reap_completions();
            for op in batch {
                let user_data = op.user_data();
                self.in_flight
                    .insert(InFlightRequest::Data(op))
                    .expect("batch validated before submission");
                if let Some(completion) = self.complete_request(user_data, -libc::EAGAIN) {
                    self.inject_completion(completion);
                }
            }
            return Ok(());
        }

        for op in batch {
            let request = InFlightRequest::Data(op);
            let entry = Self::build_entry(&self.file, &request);
            self.in_flight
                .insert(request)
                .expect("batch validated before submission");

            // SAFETY: the request and its iovecs are retained before the SQ tail
            // is published, and remain valid until the completion is consumed.
            // The whole batch fits in this exclusively borrowed submission queue.
            unsafe { sq.push(&entry) }.expect("batch fits in submission queue");
        }

        sq.sync();
        match submitter.submit() {
            Ok(_) => self.needs_submit_retry = false,
            Err(e) => {
                self.needs_submit_retry = true;
                warn!("io_uring submit failed after SQEs were published: {e}");
                self.completions.notifier().write(1).unwrap();
            }
        }
        Ok(())
    }

    fn build_entry(file: &File, request: &InFlightRequest) -> squeue::Entry {
        let fd = types::Fd(file.as_raw_fd());

        let entry = match request {
            InFlightRequest::Data(op) => {
                let iovecs = op.iovecs();
                if op.is_read() {
                    opcode::Readv::new(fd, iovecs.as_ptr(), iovecs.len() as u32)
                        .offset(op.offset() as u64)
                        .build()
                } else {
                    opcode::Writev::new(fd, iovecs.as_ptr(), iovecs.len() as u32)
                        .offset(op.offset() as u64)
                        .build()
                }
            }
            InFlightRequest::Fsync { .. } => opcode::Fsync::new(fd).build(),
        };

        entry.user_data(request.user_data())
    }

    /// Submits an fsync operation carrying `user_data`.
    pub fn submit_fsync(&mut self, user_data: u64) -> io::Result<()> {
        let request = InFlightRequest::Fsync { user_data };
        let entry = Self::build_entry(&self.file, &request);

        self.submit_kernel_entry(request, &entry)
    }

    /// Injects a completion that did not come from a kernel CQE.
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
        if self.needs_submit_retry {
            match self.io_uring.submitter().submit() {
                Ok(_) => self.needs_submit_retry = false,
                Err(e) => warn!("io_uring retry submit failed for retained SQEs: {e}"),
            }
        }

        loop {
            let entry = self.io_uring.completion().next();
            let Some(entry) = entry else { break };
            if let Some(completion) = self.complete_request(entry.user_data(), entry.result()) {
                return Some(completion);
            }
        }

        self.completions.next_completed()
    }

    fn reap_completions(&mut self) {
        if self.needs_submit_retry {
            match self.io_uring.submitter().submit() {
                Ok(_) => self.needs_submit_retry = false,
                Err(error) => warn!("io_uring retry submit failed for retained SQEs: {error}"),
            }
        }
        loop {
            let entry = self.io_uring.completion().next();
            let Some(entry) = entry else { break };
            if let Some(completion) = self.complete_request(entry.user_data(), entry.result()) {
                self.completions.complete(completion);
            }
        }
    }
}

impl Drop for UringDataIo {
    fn drop(&mut self) {
        // Closing the ring fd does not cancel io_uring ops that have started.
        // Wait for CQEs before releasing retained iovecs.
        if self.needs_submit_retry {
            if let Err(e) = self.io_uring.submitter().submit() {
                warn!("io_uring drain submit failed for retained SQEs: {e}");
            }
            self.needs_submit_retry = false;
        }

        let max_drain_iterations = self.in_flight.len().saturating_mul(2);
        let mut drain_iterations = 0;
        while !self.in_flight.is_empty() {
            if drain_iterations == max_drain_iterations {
                error!(
                    "io_uring drain abandoned with {} operations still in flight after {} drain iterations",
                    self.in_flight.len(),
                    drain_iterations
                );
                // Keep retained buffers mapped if the ring cannot be drained.
                mem::forget(mem::take(&mut self.in_flight));
                break;
            }
            drain_iterations += 1;

            if let Some(entry) = self.io_uring.completion().next() {
                self.in_flight.remove(entry.user_data());
                continue;
            }

            // No completion ready: block in the kernel until at least one is.
            if let Err(e) = self.io_uring.submitter().submit_and_wait(1) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                error!(
                    "io_uring drain abandoned with {} operations still in flight: {e}",
                    self.in_flight.len()
                );
                // Keep retained buffers mapped if the ring cannot be drained.
                mem::forget(mem::take(&mut self.in_flight));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::thread::sleep;
    use std::time::Duration;
    use std::{io, ptr};

    use io_uring::{opcode, squeue, types};
    use vmm_sys_util::tempfile::TempFile;

    use super::{InFlightRequest, UringDataIo};
    use crate::async_io::{AsyncIoCompletion, AsyncIoOperation, OwnedIoBuffer};

    fn wait_for_completion(data_io: &mut UringDataIo) -> AsyncIoCompletion {
        for _ in 0..1000 {
            if let Some(completion) = data_io.next_completion() {
                return completion;
            }
            sleep(Duration::from_millis(1));
        }

        panic!("timed out waiting for io_uring completion");
    }

    #[test]
    fn uring_rejects_socket_operations() {
        let file = TempFile::new().unwrap().into_file();
        let mut data_io = UringDataIo::new(file, 8).unwrap();
        let socket = opcode::Socket::new(libc::AF_UNIX, libc::SOCK_STREAM, 0)
            .build()
            .user_data(1);
        // SAFETY: The entry contains no pointers to userspace buffers.
        unsafe { data_io.io_uring.submission().push(&socket) }.unwrap();
        data_io.io_uring.submitter().submit().unwrap();
        let completion = wait_for_completion(&mut data_io);
        if completion.result >= 0 {
            // SAFETY: IORING_OP_SOCKET returned a new owned descriptor.
            drop(unsafe { OwnedFd::from_raw_fd(completion.result) });
        }
        assert_eq!(completion.result, -libc::EACCES);

        let connect = opcode::Connect::new(types::Fd(-1), ptr::null(), 0)
            .build()
            .user_data(2);
        // SAFETY: The invalid fd and null address refer to no userspace allocation.
        unsafe { data_io.io_uring.submission().push(&connect) }.unwrap();
        data_io.io_uring.submitter().submit().unwrap();
        assert_eq!(wait_for_completion(&mut data_io).result, -libc::EACCES);
    }

    #[test]
    fn uring_rejects_sqe_flags() {
        let file = TempFile::new().unwrap().into_file();
        let mut data_io = UringDataIo::new(file, 8).unwrap();
        let fsync = opcode::Fsync::new(types::Fd(data_io.file.as_raw_fd()))
            .build()
            .flags(squeue::Flags::ASYNC)
            .user_data(1);
        // SAFETY: The entry contains no pointers to userspace buffers.
        unsafe { data_io.io_uring.submission().push(&fsync) }.unwrap();
        data_io.io_uring.submitter().submit().unwrap();
        assert_eq!(wait_for_completion(&mut data_io).result, -libc::EACCES);

        data_io.submit_fsync(2).unwrap();
        assert_eq!(wait_for_completion(&mut data_io).result, 0);
    }

    #[test]
    fn uring_rejects_registration_changes() {
        let file = TempFile::new().unwrap().into_file();
        let data_io = UringDataIo::new(file, 8).unwrap();
        assert_eq!(
            data_io
                .io_uring
                .submitter()
                .register_eventfd(data_io.notifier().as_raw_fd())
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
    }

    #[test]
    fn uring_rejects_duplicate_user_data_for_metadata_ops() {
        let file = TempFile::new().unwrap().into_file();
        file.set_len(512).unwrap();
        let mut data_io = UringDataIo::new(file, 8).unwrap();

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

        data_io.submit_fsync(7).unwrap();
        let completion = wait_for_completion(&mut data_io);
        assert_eq!(completion.user_data, 7);
        assert_eq!(completion.result, 0);
    }

    #[test]
    fn uring_drop_drains_in_flight_operations() {
        let file = TempFile::new().unwrap().into_file();
        file.set_len(8192).unwrap();
        let mut data_io = UringDataIo::new(file, 8).unwrap();

        for user_data in 0..4 {
            data_io
                .submit_operation(AsyncIoOperation::read_to_vec(
                    0,
                    OwnedIoBuffer::from_vec(vec![0; 512]),
                    user_data,
                ))
                .unwrap();
        }

        drop(data_io);
    }

    #[test]
    fn uring_queue_full_batch_retries_each_operation() {
        let file = TempFile::new().unwrap().into_file();
        file.set_len(512).unwrap();
        let mut data_io = UringDataIo::new(file, 1).unwrap();
        let available = {
            let (_, sq, _) = data_io.io_uring.split();
            sq.capacity() - sq.len()
        };
        let batch_len = available + 1;
        let batch: Vec<_> = (0..batch_len as u64)
            .map(|user_data| {
                AsyncIoOperation::read_to_vec(0, OwnedIoBuffer::from_vec(vec![0; 512]), user_data)
            })
            .collect();

        data_io.submit_batch(batch).unwrap();

        let mut completed = Vec::new();
        for _ in 0..batch_len {
            let completion = wait_for_completion(&mut data_io);
            assert_eq!(completion.result, 512);
            assert!(completion.buffer.is_some());
            completed.push(completion.user_data);
        }
        completed.sort_unstable();
        assert_eq!(completed, (0..batch_len as u64).collect::<Vec<_>>());
    }

    #[test]
    fn retries_retained_data_and_fsync_requests() {
        use std::os::unix::fs::FileExt;

        let file = TempFile::new().unwrap().into_file();
        let mut data_io = UringDataIo::new(file, 8).unwrap();
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
    fn published_retry_is_submitted_without_a_second_entry() {
        let file = TempFile::new().unwrap().into_file();
        file.set_len(512).unwrap();
        let mut data_io = UringDataIo::new(file, 8).unwrap();
        data_io
            .in_flight
            .insert(InFlightRequest::Data(AsyncIoOperation::read_to_vec(
                0,
                OwnedIoBuffer::from_vec(vec![0; 512]),
                7,
            )))
            .unwrap();

        let ring = &mut data_io.io_uring;
        let file = &data_io.file;
        let needs_submit_retry = &mut data_io.needs_submit_retry;
        assert!(
            data_io
                .in_flight
                .complete(7, -libc::EAGAIN, |request| {
                    let entry = UringDataIo::build_entry(file, request);
                    // SAFETY: complete retains this request until the CQE is consumed.
                    unsafe { ring.submission().push(&entry) }.unwrap();
                    // Model a submit syscall failure after publishing the SQE.
                    *needs_submit_retry = true;
                    Ok(())
                })
                .is_none()
        );
        assert_eq!(data_io.io_uring.submission().len(), 1);

        let completion = wait_for_completion(&mut data_io);
        assert_eq!(completion.user_data, 7);
        assert_eq!(completion.result, 512);
        assert!(!data_io.needs_submit_retry);
        assert!(data_io.in_flight.is_empty());
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
        let mut data_io = UringDataIo::new(file, 8).unwrap();
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
