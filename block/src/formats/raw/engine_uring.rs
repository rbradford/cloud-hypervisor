// Copyright © 2021 Intel Corporation
//
// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use std::fs::File;

use vmm_sys_util::eventfd::EventFd;

use crate::async_io::{
    AsyncIo, AsyncIoCompletion, AsyncIoError, AsyncIoOperation, AsyncIoResult, UringDataIo,
};
use crate::error::{BlockError, BlockErrorKind, BlockResult};
use crate::sparse::{punch_hole, write_zeroes};
use crate::{SECTOR_SIZE, is_block_device, probe_direct_alignment};

pub(crate) struct RawAsync {
    data_io: UringDataIo,
    raw_file: File,
    alignment: Option<u64>,
    is_block_device: bool,
}

impl RawAsync {
    pub(crate) fn new(raw_file: File, direct: bool, ring_depth: u32) -> BlockResult<Self> {
        let data_io = UringDataIo::new(raw_file.try_clone()?, ring_depth)
            .map_err(|e| BlockError::new(BlockErrorKind::Io, e))?;
        let is_block_device = is_block_device(&raw_file);
        let alignment = if direct {
            Some(probe_direct_alignment(&raw_file).unwrap_or(SECTOR_SIZE))
        } else {
            None
        };

        Ok(RawAsync {
            data_io,
            raw_file,
            alignment,
            is_block_device,
        })
    }
}

impl AsyncIo for RawAsync {
    fn notifier(&self) -> &EventFd {
        self.data_io.notifier()
    }

    fn alignment(&self) -> Option<u64> {
        self.alignment
    }

    fn submit_data_operation(&mut self, op: AsyncIoOperation) -> AsyncIoResult<()> {
        let is_read = op.is_read();

        self.data_io.submit_operation(op).map_err(|e| {
            if is_read {
                AsyncIoError::ReadVectored(e)
            } else {
                AsyncIoError::WriteVectored(e)
            }
        })
    }

    fn fsync(&mut self, user_data: Option<u64>) -> AsyncIoResult<()> {
        if let Some(user_data) = user_data {
            self.data_io
                .submit_fsync(user_data)
                .map_err(AsyncIoError::Fsync)?;
        } else {
            self.raw_file.sync_all().map_err(AsyncIoError::Fsync)?;
        }

        Ok(())
    }

    fn next_completed_request(&mut self) -> Option<AsyncIoCompletion> {
        self.data_io.next_completion()
    }

    fn batch_requests_enabled(&self) -> bool {
        true
    }

    fn submit_batch_requests(&mut self, batch_request: Vec<AsyncIoOperation>) -> AsyncIoResult<()> {
        self.data_io
            .submit_batch(batch_request)
            .map_err(AsyncIoError::SubmitBatchRequests)
    }

    fn punch_hole(&mut self, offset: u64, length: u64, user_data: u64) -> AsyncIoResult<()> {
        // Run synchronously rather than submitting a fallocate request through
        // the ring. This avoids reaping ENOTSUPP in the completion routine and
        // reissuing the request, and lets the sparse helper handle the ioctl
        // path for block devices and the write fallback for unsupported
        // filesystems.
        punch_hole(&mut self.raw_file, self.is_block_device, offset, length)
            .map_err(AsyncIoError::PunchHole)?;
        // Deliver the completion through the normal io_uring path by
        // queuing a NOP carrying `user_data`. The registered eventfd will
        // fire when it completes, just like any other request.
        self.data_io
            .submit_nop(user_data)
            .map_err(AsyncIoError::PunchHole)
    }

    fn write_zeroes(&mut self, offset: u64, length: u64, user_data: u64) -> AsyncIoResult<()> {
        // Same rationale as punch_hole().
        write_zeroes(&mut self.raw_file, self.is_block_device, offset, length)
            .map_err(AsyncIoError::WriteZeroes)?;
        self.data_io
            .submit_nop(user_data)
            .map_err(AsyncIoError::WriteZeroes)
    }
}
