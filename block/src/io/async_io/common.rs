// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! Helpers used by both aio and uring async io.

use std::collections::HashMap;
#[cfg(feature = "io_uring")]
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io;

use super::{AsyncIoOperation, OwnedIoBuffer};

/// Retains the operation and identifier for an outstanding request.
pub(super) enum InFlightRequest {
    Data(AsyncIoOperation),
    Fsync { user_data: u64 },
}

impl InFlightRequest {
    pub(super) fn user_data(&self) -> u64 {
        match self {
            Self::Data(op) => op.user_data(),
            Self::Fsync { user_data } => *user_data,
        }
    }

    pub(super) fn into_completion_buffer(self) -> Option<OwnedIoBuffer> {
        match self {
            Self::Data(op) => op.into_completion_buffer(),
            Self::Fsync { .. } => None,
        }
    }
}

/// Converts an I/O error into the negative errno form used in completions.
pub(super) fn errno_result(error: &io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}

/// Tracks request identifiers and retains requests until completion.
#[derive(Default)]
pub(super) struct InFlightRequests {
    requests: HashMap<u64, InFlightRequest>,
}

impl InFlightRequests {
    /// Validates that a batch has unique `user_data` not already in flight.
    #[cfg(feature = "io_uring")]
    pub(super) fn validate_batch(&self, batch: &[AsyncIoOperation]) -> io::Result<()> {
        let mut seen = HashSet::with_capacity(batch.len());

        for op in batch {
            let user_data = op.user_data();
            if self.requests.contains_key(&user_data) || !seen.insert(user_data) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("duplicate async I/O user_data {user_data}"),
                ));
            }
        }

        Ok(())
    }

    pub(super) fn insert(&mut self, request: InFlightRequest) -> io::Result<()> {
        let user_data = request.user_data();
        match self.requests.entry(user_data) {
            Entry::Vacant(entry) => {
                entry.insert(request);
                Ok(())
            }
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate async I/O user_data {user_data}"),
            )),
        }
    }

    pub(super) fn remove(&mut self, user_data: u64) -> Option<InFlightRequest> {
        self.requests.remove(&user_data)
    }

    #[cfg(feature = "io_uring")]
    pub(super) fn len(&self) -> usize {
        self.requests.len()
    }

    #[cfg(feature = "io_uring")]
    pub(super) fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{InFlightRequest, InFlightRequests};
    use crate::async_io::{AsyncIoOperation, OwnedIoBuffer};

    #[test]
    fn duplicate_identifier_preserves_original_request() {
        let mut requests = InFlightRequests::default();
        requests
            .insert(InFlightRequest::Data(AsyncIoOperation::read_to_vec(
                0,
                OwnedIoBuffer::from_vec(vec![0xa5; 8]),
                7,
            )))
            .unwrap();

        let duplicates = [
            InFlightRequest::Data(AsyncIoOperation::read_to_vec(
                0,
                OwnedIoBuffer::from_vec(vec![0; 8]),
                7,
            )),
            InFlightRequest::Fsync { user_data: 7 },
        ];
        for request in duplicates {
            assert_eq!(
                requests.insert(request).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
        }

        let buffer = requests
            .remove(7)
            .unwrap()
            .into_completion_buffer()
            .unwrap();
        assert_eq!(buffer.as_slice(), &[0xa5; 8]);

        requests
            .insert(InFlightRequest::Fsync { user_data: 7 })
            .unwrap();
        assert!(matches!(
            requests.remove(7),
            Some(InFlightRequest::Fsync { user_data: 7 })
        ));
    }

    #[test]
    #[cfg(feature = "io_uring")]
    fn batch_validation_rejects_duplicates_without_reserving_identifiers() {
        let mut requests = InFlightRequests::default();
        requests
            .insert(InFlightRequest::Fsync { user_data: 7 })
            .unwrap();

        for identifiers in [[8, 7], [8, 8]] {
            let batch: Vec<_> = identifiers
                .into_iter()
                .map(|user_data| {
                    AsyncIoOperation::read_to_vec(0, OwnedIoBuffer::from_vec(vec![0; 8]), user_data)
                })
                .collect();
            assert_eq!(
                requests.validate_batch(&batch).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            requests.validate_batch(&batch[..1]).unwrap();
            assert_eq!(requests.len(), 1);
        }
    }
}
