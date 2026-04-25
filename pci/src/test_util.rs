// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::sync::Arc;

use vm_device::interrupt::{InterruptIndex, InterruptSourceConfig, InterruptSourceGroup};
use vmm_sys_util::eventfd::EventFd;

use crate::{MaybeMutInterruptSourceGroup, MsixConfig};

pub(crate) struct TestInterruptSourceGroup;

impl InterruptSourceGroup for TestInterruptSourceGroup {
    fn trigger(&self, _index: InterruptIndex) -> io::Result<()> {
        Ok(())
    }

    fn notifier(&self, _index: InterruptIndex) -> Option<EventFd> {
        None
    }

    fn update(
        &self,
        _index: InterruptIndex,
        _config: InterruptSourceConfig,
        _masked: bool,
        _set_gsi: bool,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_gsi(&self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn msix_config() -> MsixConfig {
    MsixConfig::new(
        1,
        MaybeMutInterruptSourceGroup::Immutable(Arc::new(TestInterruptSourceGroup)),
        0,
        None,
    )
    .unwrap()
}
