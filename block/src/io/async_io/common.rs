// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! Helpers used by both aio and uring async io.

use std::collections::HashMap;
#[cfg(feature = "io_uring")]
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io;

use super::{AsyncIoCompletion, AsyncIoOperation, OwnedIoBuffer};

const MAX_RETRIES: u8 = 3;

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
    requests: HashMap<u64, (InFlightRequest, u8)>,
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
                entry.insert((request, 0));
                Ok(())
            }
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate async I/O user_data {user_data}"),
            )),
        }
    }

    pub(super) fn remove(&mut self, user_data: u64) -> Option<InFlightRequest> {
        self.requests.remove(&user_data).map(|(request, _)| request)
    }

    /// Completes a finished attempt, retrying transient errors up to three times.
    ///
    /// The caller must have consumed the kernel completion or established that
    /// submission was rejected. `retry` must return an error only if it did not
    /// publish or submit the request. A successful retry keeps the request and
    /// its buffers retained until the next completion.
    pub(super) fn complete(
        &mut self,
        user_data: u64,
        mut result: i32,
        mut retry: impl FnMut(&InFlightRequest) -> io::Result<()>,
    ) -> Option<AsyncIoCompletion> {
        if let Some((request, retries)) = self.requests.get_mut(&user_data) {
            while (result == -libc::EAGAIN || result == -libc::EINTR) && *retries < MAX_RETRIES {
                *retries += 1;
                match retry(request) {
                    Ok(()) => return None,
                    Err(error) => result = errno_result(&error),
                }
            }
        }

        let buffer = self
            .remove(user_data)
            .and_then(InFlightRequest::into_completion_buffer);
        Some(AsyncIoCompletion::new(user_data, result, buffer))
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
    fn retries_keep_the_original_buffer_and_identifier() {
        for error in [libc::EAGAIN, libc::EINTR] {
            let mut requests = InFlightRequests::default();
            let op = AsyncIoOperation::read_to_vec(512, OwnedIoBuffer::from_vec(vec![0xa5; 8]), 7);
            let iovecs = op.iovecs().as_ptr();
            let buffer = op.iovecs()[0].iov_base;
            requests.insert(InFlightRequest::Data(op)).unwrap();

            assert!(
                requests
                    .complete(7, -error, |request| {
                        let InFlightRequest::Data(op) = request else {
                            panic!("expected data request")
                        };
                        assert_eq!(op.user_data(), 7);
                        assert_eq!(op.offset(), 512);
                        assert_eq!(op.iovecs().as_ptr(), iovecs);
                        assert_eq!(op.iovecs()[0].iov_base, buffer);
                        Ok(())
                    })
                    .is_none()
            );
            assert_eq!(
                requests
                    .insert(InFlightRequest::Fsync { user_data: 7 })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );

            let completion = requests
                .complete(7, 8, |_| panic!("successful I/O retried"))
                .unwrap();
            assert_eq!(completion.result, 8);
            assert_eq!(completion.buffer.unwrap().as_slice(), &[0xa5; 8]);
            assert!(requests.remove(7).is_none());
        }
    }

    #[test]
    fn retry_limit_spans_kernel_completions() {
        let mut requests = InFlightRequests::default();
        requests
            .insert(InFlightRequest::Fsync { user_data: 7 })
            .unwrap();
        let mut attempts = 0;
        for error in [libc::EAGAIN, libc::EINTR, libc::EAGAIN] {
            assert!(
                requests
                    .complete(7, -error, |request| {
                        assert!(matches!(request, InFlightRequest::Fsync { user_data: 7 }));
                        attempts += 1;
                        Ok(())
                    })
                    .is_none()
            );
        }
        let completion = requests
            .complete(7, -libc::EINTR, |_| panic!("retry limit exceeded"))
            .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(completion.result, -libc::EINTR);
        assert!(requests.remove(7).is_none());
    }

    #[test]
    fn rejected_resubmissions_are_bounded() {
        for error in [libc::EAGAIN, libc::EINTR, libc::EIO] {
            let mut requests = InFlightRequests::default();
            requests
                .insert(InFlightRequest::Fsync { user_data: 7 })
                .unwrap();
            let mut attempts = 0;
            let completion = requests
                .complete(7, -libc::EAGAIN, |_| {
                    attempts += 1;
                    Err(io::Error::from_raw_os_error(error))
                })
                .unwrap();
            assert_eq!(attempts, if error == libc::EIO { 1 } else { 3 });
            assert_eq!(completion.result, -error);
            assert!(requests.remove(7).is_none());
        }
    }

    #[test]
    fn permanent_errors_and_short_io_are_not_retried() {
        for result in [-libc::EIO, 0, 4] {
            let mut requests = InFlightRequests::default();
            requests
                .insert(InFlightRequest::Data(AsyncIoOperation::read_to_vec(
                    0,
                    OwnedIoBuffer::from_vec(vec![0; 8]),
                    7,
                )))
                .unwrap();
            let completion = requests
                .complete(7, result, |_| panic!("unexpected retry"))
                .unwrap();
            assert_eq!(completion.result, result);
            assert!(completion.buffer.is_some());
            assert!(requests.remove(7).is_none());
        }
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
