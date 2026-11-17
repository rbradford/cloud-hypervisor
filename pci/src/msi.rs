// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//

use std::sync::Arc;
use std::{io, result};

use log::{debug, error};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use vm_device::interrupt::{
    InterruptIndex, InterruptSourceConfig, InterruptSourceGroup, MsiIrqSourceConfig,
};
use vm_migration::{MigratableError, Pausable, Snapshot, Snapshottable};

// MSI control masks
const MSI_CTL_ENABLE: u16 = 0x1;
const MSI_CTL_MULTI_MSG_ENABLE: u16 = 0x70;
const MSI_CTL_64_BITS: u16 = 0x80;
const MSI_CTL_PER_VECTOR: u16 = 0x100;

// MSI message offsets
const MSI_MSG_CTL_OFFSET: u64 = 0x2;
const MSI_MSG_ADDR_LO_OFFSET: u64 = 0x4;

// MSI message masks
const MSI_MSG_ADDR_LO_MASK: u32 = 0xffff_fffc;

pub fn msi_num_enabled_vectors(msg_ctl: u16) -> usize {
    let field = (msg_ctl >> 4) & 0x7;

    if field > 5 {
        return 0;
    }

    1 << field
}

pub(crate) fn msi_num_capable_vectors(msg_ctl: u16) -> usize {
    let field = (msg_ctl >> 1) & 0x7;

    if field > 5 {
        return 0;
    }

    1 << field
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed enabling the interrupt route")]
    EnableInterruptRoute(#[source] io::Error),
    #[error("Failed updating the interrupt route")]
    UpdateInterruptRoute(#[source] io::Error),
}

pub(crate) const MSI_CONFIG_ID: &str = "msi_config";

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct MsiCap {
    // Message Control Register
    //   0:     MSI enable.
    //   3-1;   Multiple message capable.
    //   6-4:   Multiple message enable.
    //   7:     64 bits address capable.
    //   8:     Per-vector masking capable.
    //   15-9:  Reserved.
    pub msg_ctl: u16,
    // Message Address (LSB)
    //   1-0:  Reserved.
    //   31-2: Message address.
    pub msg_addr_lo: u32,
    // Message Upper Address (MSB)
    //   31-0: Message address.
    pub msg_addr_hi: u32,
    // Message Data
    //   15-0: Message data.
    pub msg_data: u16,
    // Mask Bits
    //   31-0: Mask bits.
    pub mask_bits: u32,
    // Pending Bits
    //   31-0: Pending bits.
    pub pending_bits: u32,
}

impl MsiCap {
    fn addr_64_bits(&self) -> bool {
        self.msg_ctl & MSI_CTL_64_BITS == MSI_CTL_64_BITS
    }

    fn per_vector_mask(&self) -> bool {
        self.msg_ctl & MSI_CTL_PER_VECTOR == MSI_CTL_PER_VECTOR
    }

    fn enabled(&self) -> bool {
        self.msg_ctl & MSI_CTL_ENABLE == MSI_CTL_ENABLE
    }

    fn num_enabled_vectors(&self) -> usize {
        msi_num_enabled_vectors(self.msg_ctl)
    }

    fn vector_masked(&self, vector: usize) -> bool {
        if !self.per_vector_mask() {
            return false;
        }

        (self.mask_bits >> vector) & 0x1 == 0x1
    }

    fn size(&self) -> u64 {
        let mut size: u64 = 0xa;

        if self.addr_64_bits() {
            size += 0x4;
        }
        if self.per_vector_mask() {
            size += 0xa;
        }

        size
    }

    fn update(&mut self, offset: u64, data: &[u8]) {
        // Calculate message data offset depending on the address being 32 or
        // 64 bits.
        // Calculate upper address offset if the address is 64 bits.
        // Calculate mask bits offset based on the address being 32 or 64 bits
        // and based on the per vector masking being enabled or not.
        let (msg_data_offset, addr_hi_offset, mask_bits_offset): (u64, Option<u64>, Option<u64>) =
            if self.addr_64_bits() {
                let mask_bits = if self.per_vector_mask() {
                    Some(0x10)
                } else {
                    None
                };
                (0xc, Some(0x8), mask_bits)
            } else {
                let mask_bits = if self.per_vector_mask() {
                    Some(0xc)
                } else {
                    None
                };
                (0x8, None, mask_bits)
            };

        // Update cache without overriding the read-only bits.
        match data.len() {
            2 => {
                let value = u16::from_le_bytes(data.try_into().expect("data should be 2 bytes"));
                match offset {
                    MSI_MSG_CTL_OFFSET => {
                        self.msg_ctl = (self.msg_ctl
                            & !(MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                            | (value & (MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE));
                    }
                    x if x == msg_data_offset => self.msg_data = value,
                    _ => debug!("invalid offset"),
                }
            }
            4 => {
                let value = u32::from_le_bytes(data.try_into().expect("data should be 4 bytes"));
                match offset {
                    0x0 => {
                        self.msg_ctl = (self.msg_ctl
                            & !(MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE))
                            | ((value >> 16) as u16 & (MSI_CTL_ENABLE | MSI_CTL_MULTI_MSG_ENABLE));
                    }
                    MSI_MSG_ADDR_LO_OFFSET => self.msg_addr_lo = value & MSI_MSG_ADDR_LO_MASK,
                    x if x == msg_data_offset => self.msg_data = value as u16,
                    x if addr_hi_offset.is_some() && x == addr_hi_offset.unwrap() => {
                        self.msg_addr_hi = value;
                    }
                    x if mask_bits_offset.is_some() && x == mask_bits_offset.unwrap() => {
                        self.mask_bits = value;
                    }
                    _ => debug!("invalid offset"),
                }
            }
            _ => debug!("invalid data length"),
        }

        if matches!((offset, data.len()), (MSI_MSG_CTL_OFFSET, 2) | (0, 4)) {
            let capable = (self.msg_ctl >> 1) & 0x7;
            let enabled = ((self.msg_ctl >> 4) & 0x7).min(capable);
            self.msg_ctl = (self.msg_ctl & !MSI_CTL_MULTI_MSG_ENABLE) | (enabled << 4);
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct MsiConfigState {
    cap: MsiCap,
}

pub struct MsiConfig {
    pub cap: MsiCap,
    interrupt_source_group: Arc<dyn InterruptSourceGroup>,
}

impl MsiConfig {
    pub fn new(
        msg_ctl: u16,
        interrupt_source_group: Arc<dyn InterruptSourceGroup>,
        state: Option<MsiConfigState>,
    ) -> Result<Self, Error> {
        let cap = if let Some(state) = state {
            if state.cap.enabled() {
                let nvec = state.cap.num_enabled_vectors();
                for idx in 0..nvec {
                    let config = MsiIrqSourceConfig {
                        high_addr: state.cap.msg_addr_hi,
                        low_addr: state.cap.msg_addr_lo,
                        data: (u32::from(state.cap.msg_data) & !((nvec - 1) as u32)) | idx as u32,
                        devid: 0,
                    };

                    interrupt_source_group
                        .update(
                            idx as InterruptIndex,
                            InterruptSourceConfig::MsiIrq(config),
                            state.cap.vector_masked(idx),
                            false,
                        )
                        .map_err(Error::UpdateInterruptRoute)?;
                }

                interrupt_source_group
                    .set_gsi()
                    .map_err(Error::EnableInterruptRoute)?;

                interrupt_source_group
                    .enable()
                    .map_err(Error::EnableInterruptRoute)?;
            }

            state.cap
        } else {
            MsiCap {
                msg_ctl,
                ..Default::default()
            }
        };

        Ok(MsiConfig {
            cap,
            interrupt_source_group,
        })
    }

    fn state(&self) -> MsiConfigState {
        MsiConfigState { cap: self.cap }
    }

    pub fn enabled(&self) -> bool {
        self.cap.enabled()
    }

    pub fn size(&self) -> u64 {
        self.cap.size()
    }

    pub fn num_enabled_vectors(&self) -> usize {
        self.cap.num_enabled_vectors()
    }

    pub fn update(&mut self, offset: u64, data: &[u8]) {
        let old_enabled = self.cap.enabled();

        self.cap.update(offset, data);

        if self.cap.enabled() {
            let nvec = self.num_enabled_vectors();
            for idx in 0..nvec {
                let config = MsiIrqSourceConfig {
                    high_addr: self.cap.msg_addr_hi,
                    low_addr: self.cap.msg_addr_lo,
                    data: (u32::from(self.cap.msg_data) & !((nvec - 1) as u32)) | idx as u32,
                    devid: 0,
                };

                if let Err(e) = self.interrupt_source_group.update(
                    idx as InterruptIndex,
                    InterruptSourceConfig::MsiIrq(config),
                    self.cap.vector_masked(idx),
                    true,
                ) {
                    error!("Failed updating vector: {e:?}");
                }
            }

            if !old_enabled && let Err(e) = self.interrupt_source_group.enable() {
                error!("Failed enabling irq_fd: {e:?}");
            }
        } else if old_enabled && let Err(e) = self.interrupt_source_group.disable() {
            error!("Failed disabling irq_fd: {e:?}");
        }
    }
}

impl Pausable for MsiConfig {}

impl Snapshottable for MsiConfig {
    fn id(&self) -> String {
        String::from(MSI_CONFIG_ID)
    }

    fn snapshot(&mut self) -> result::Result<Snapshot, MigratableError> {
        Snapshot::new_from_state(&self.state())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use vmm_sys_util::eventfd::EventFd;

    use super::*;

    #[derive(Default)]
    struct TestInterruptSourceGroup {
        updates: Mutex<Vec<(InterruptIndex, MsiIrqSourceConfig, bool, bool)>>,
    }

    impl InterruptSourceGroup for TestInterruptSourceGroup {
        fn trigger(&self, _index: InterruptIndex) -> io::Result<()> {
            unreachable!()
        }

        fn notifier(&self, _index: InterruptIndex) -> Option<EventFd> {
            unreachable!()
        }

        fn update(
            &self,
            index: InterruptIndex,
            config: InterruptSourceConfig,
            masked: bool,
            set_gsi: bool,
        ) -> io::Result<()> {
            let InterruptSourceConfig::MsiIrq(config) = config else {
                unreachable!()
            };
            self.updates
                .lock()
                .unwrap()
                .push((index, config, masked, set_gsi));
            Ok(())
        }

        fn set_gsi(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn check_msi_message_data(restored: bool) {
        let cases: &[(u16, u16, &[u32])] = &[
            (0, 0xa55f, &[0xa55f]),
            (1, 0xa55f, &[0xa55e, 0xa55f]),
            (2, 0x43, &[0x40, 0x41, 0x42, 0x43]),
            (
                3,
                0xa55f,
                &[
                    0xa558, 0xa559, 0xa55a, 0xa55b, 0xa55c, 0xa55d, 0xa55e, 0xa55f,
                ],
            ),
            (
                4,
                0xa55f,
                &[
                    0xa550, 0xa551, 0xa552, 0xa553, 0xa554, 0xa555, 0xa556, 0xa557, 0xa558, 0xa559,
                    0xa55a, 0xa55b, 0xa55c, 0xa55d, 0xa55e, 0xa55f,
                ],
            ),
            (
                5,
                0xa55f,
                &[
                    0xa540, 0xa541, 0xa542, 0xa543, 0xa544, 0xa545, 0xa546, 0xa547, 0xa548, 0xa549,
                    0xa54a, 0xa54b, 0xa54c, 0xa54d, 0xa54e, 0xa54f, 0xa550, 0xa551, 0xa552, 0xa553,
                    0xa554, 0xa555, 0xa556, 0xa557, 0xa558, 0xa559, 0xa55a, 0xa55b, 0xa55c, 0xa55d,
                    0xa55e, 0xa55f,
                ],
            ),
        ];
        for &(exponent, data, expected) in cases {
            let group = Arc::new(TestInterruptSourceGroup::default());
            let control = 0x180 | (exponent << 1);
            if restored {
                let state = MsiConfigState {
                    cap: MsiCap {
                        msg_ctl: control | (exponent << 4) | 1,
                        msg_addr_lo: 0xfee0_0000,
                        msg_addr_hi: 0x1234,
                        msg_data: data,
                        mask_bits: 0xaaaa_aaaa,
                        pending_bits: 0,
                    },
                };
                MsiConfig::new(
                    control,
                    Arc::clone(&group) as Arc<dyn InterruptSourceGroup>,
                    Some(state),
                )
                .unwrap();
            } else {
                let mut config = MsiConfig::new(
                    control,
                    Arc::clone(&group) as Arc<dyn InterruptSourceGroup>,
                    None,
                )
                .unwrap();
                config.update(4, &0xfee0_0000u32.to_le_bytes());
                config.update(8, &0x1234u32.to_le_bytes());
                config.update(0xc, &data.to_le_bytes());
                config.update(0x10, &0xaaaa_aaaau32.to_le_bytes());
                config.update(2, &((exponent << 4) | 1).to_le_bytes());
            }
            let updates = group.updates.lock().unwrap();
            assert_eq!(updates.len(), expected.len());
            for (index, (&data, &(vector, ref route, masked, set_gsi))) in
                expected.iter().zip(updates.iter()).enumerate()
            {
                assert_eq!(vector as usize, index);
                assert_eq!(route.data, data, "exponent {exponent}, vector {index}");
                assert_eq!(route.low_addr, 0xfee0_0000);
                assert_eq!(route.high_addr, 0x1234);
                assert_eq!(route.devid, 0);
                assert_eq!(masked, index % 2 == 1);
                assert_eq!(set_gsi, !restored);
            }
        }
    }

    fn check_msi_mme_write(width: usize) {
        let limits = [
            [0u16, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 1, 1, 1, 1, 1, 1],
            [0, 1, 2, 2, 2, 2, 2, 2],
            [0, 1, 2, 3, 3, 3, 3, 3],
            [0, 1, 2, 3, 4, 4, 4, 4],
            [0, 1, 2, 3, 4, 5, 5, 5],
        ];
        let counts = [1, 2, 4, 8, 16, 32];
        for (capable, expected) in limits.iter().enumerate() {
            let group = Arc::new(TestInterruptSourceGroup::default());
            let read_only = 0xa180 | ((capable as u16) << 1);
            let mut config = MsiConfig::new(
                read_only,
                Arc::clone(&group) as Arc<dyn InterruptSourceGroup>,
                None,
            )
            .unwrap();
            for (request, &enabled) in expected.iter().enumerate() {
                group.updates.lock().unwrap().clear();
                let control = !0x70 | ((request as u16) << 4);
                if width == 2 {
                    config.update(2, &control.to_le_bytes());
                } else {
                    config.update(0, &((u32::from(control) << 16) | 0xffff).to_le_bytes());
                }
                assert_eq!(config.cap.msg_ctl & !0x71, read_only);
                assert_eq!(
                    (config.cap.msg_ctl >> 4) & 7,
                    enabled,
                    "MMC {capable}, MME {request}, width {width}"
                );
                assert_eq!(
                    group.updates.lock().unwrap().len(),
                    counts[enabled as usize]
                );
            }
        }
    }

    #[test]
    fn msi_word_write_limits_mme_to_capability() {
        check_msi_mme_write(2);
    }

    #[test]
    fn msi_dword_write_limits_mme_to_capability() {
        check_msi_mme_write(4);
    }

    #[test]
    fn msi_capable_vector_count_decodes_valid_and_reserved_fields() {
        for (field, expected) in [
            (0u16, 1),
            (1, 2),
            (2, 4),
            (3, 8),
            (4, 16),
            (5, 32),
            (6, 0),
            (7, 0),
        ] {
            assert_eq!(msi_num_capable_vectors(0xfff1 | (field << 1)), expected);
        }
    }

    #[test]
    fn msi_live_routes_encode_vector_in_message_data() {
        check_msi_message_data(false);
    }

    #[test]
    fn msi_restored_routes_encode_vector_in_message_data() {
        check_msi_message_data(true);
    }
}
