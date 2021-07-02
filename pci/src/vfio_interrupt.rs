// Copyright © 2021 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//

use crate::{MsiConfig, MsixCap, MsixConfig, PciCapabilityId, MSIX_TABLE_ENTRY_SIZE};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;
use vm_device::interrupt::InterruptSourceGroup;

pub(crate) enum InterruptUpdateAction {
    EnableMsi,
    DisableMsi,
    EnableMsix,
    DisableMsix,
}

pub(crate) struct VfioIntx {
    pub(crate) interrupt_source_group: Arc<Box<dyn InterruptSourceGroup>>,
    pub(crate) enabled: bool,
}

pub(crate) struct VfioMsi {
    pub(crate) cfg: MsiConfig,
    pub(crate) cap_offset: u32,
    pub(crate) interrupt_source_group: Arc<Box<dyn InterruptSourceGroup>>,
}

impl VfioMsi {
    fn update(&mut self, offset: u64, data: &[u8]) -> Option<InterruptUpdateAction> {
        let old_enabled = self.cfg.enabled();

        self.cfg.update(offset, data);

        let new_enabled = self.cfg.enabled();

        if !old_enabled && new_enabled {
            return Some(InterruptUpdateAction::EnableMsi);
        }

        if old_enabled && !new_enabled {
            return Some(InterruptUpdateAction::DisableMsi);
        }

        None
    }
}

pub(crate) struct VfioMsix {
    pub(crate) bar: MsixConfig,
    pub(crate) cap: MsixCap,
    pub(crate) cap_offset: u32,
    pub(crate) interrupt_source_group: Arc<Box<dyn InterruptSourceGroup>>,
}

impl VfioMsix {
    fn update(&mut self, offset: u64, data: &[u8]) -> Option<InterruptUpdateAction> {
        let old_enabled = self.bar.enabled();

        // Update "Message Control" word
        if offset == 2 && data.len() == 2 {
            self.bar.set_msg_ctl(LittleEndian::read_u16(data));
        }

        let new_enabled = self.bar.enabled();

        if !old_enabled && new_enabled {
            return Some(InterruptUpdateAction::EnableMsix);
        }

        if old_enabled && !new_enabled {
            return Some(InterruptUpdateAction::DisableMsix);
        }

        None
    }

    fn table_accessed(&self, bar_index: u32, offset: u64) -> bool {
        let table_offset: u64 = u64::from(self.cap.table_offset());
        let table_size: u64 = u64::from(self.cap.table_size()) * (MSIX_TABLE_ENTRY_SIZE as u64);
        let table_bir: u32 = self.cap.table_bir();

        bar_index == table_bir && offset >= table_offset && offset < table_offset + table_size
    }
}

pub(crate) struct Interrupt {
    pub(crate) intx: Option<VfioIntx>,
    pub(crate) msi: Option<VfioMsi>,
    pub(crate) msix: Option<VfioMsix>,
}

impl Interrupt {
    pub(crate) fn update_msi(&mut self, offset: u64, data: &[u8]) -> Option<InterruptUpdateAction> {
        if let Some(ref mut msi) = &mut self.msi {
            let action = msi.update(offset, data);
            return action;
        }

        None
    }

    pub(crate) fn update_msix(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> Option<InterruptUpdateAction> {
        if let Some(ref mut msix) = &mut self.msix {
            let action = msix.update(offset, data);
            return action;
        }

        None
    }

    pub(crate) fn accessed(&self, offset: u64) -> Option<(PciCapabilityId, u64)> {
        if let Some(msi) = &self.msi {
            if offset >= u64::from(msi.cap_offset)
                && offset < u64::from(msi.cap_offset) + msi.cfg.size()
            {
                return Some((
                    PciCapabilityId::MessageSignalledInterrupts,
                    u64::from(msi.cap_offset),
                ));
            }
        }

        if let Some(msix) = &self.msix {
            if offset == u64::from(msix.cap_offset) {
                return Some((PciCapabilityId::MsiX, u64::from(msix.cap_offset)));
            }
        }

        None
    }

    pub(crate) fn msix_table_accessed(&self, bar_index: u32, offset: u64) -> bool {
        if let Some(msix) = &self.msix {
            return msix.table_accessed(bar_index, offset);
        }

        false
    }

    pub(crate) fn msix_write_table(&mut self, offset: u64, data: &[u8]) {
        if let Some(ref mut msix) = &mut self.msix {
            let offset = offset - u64::from(msix.cap.table_offset());
            msix.bar.write_table(offset, data)
        }
    }

    pub(crate) fn msix_read_table(&self, offset: u64, data: &mut [u8]) {
        if let Some(msix) = &self.msix {
            let offset = offset - u64::from(msix.cap.table_offset());
            msix.bar.read_table(offset, data)
        }
    }

    pub(crate) fn intx_in_use(&self) -> bool {
        if let Some(intx) = &self.intx {
            return intx.enabled;
        }

        false
    }
}
