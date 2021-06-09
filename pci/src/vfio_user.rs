// Copyright © 2021 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::vfio::{Interrupt, Vfio, VfioCommon};
use crate::{BarReprogrammingParams, PciBarRegionType};
use crate::{
    PciClassCode, PciConfiguration, PciDevice, PciDeviceError, PciHeaderType, PciSubclass,
};
use std::any::Any;
use std::os::unix::prelude::AsRawFd;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::{Arc, Barrier, Mutex};
use std::u32;
use vfio_bindings::bindings::vfio::*;
use vfio_ioctls::{VfioError, VfioIrq};
use vfio_user::{Client, Error as VfioUserError};
use vm_allocator::SystemAllocator;
use vm_device::interrupt::{InterruptManager, InterruptSourceGroup, MsiIrqGroupConfig};
use vm_device::BusDevice;
use vm_memory::bitmap::AtomicBitmap;
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestUsize};
use vmm_sys_util::eventfd::EventFd;

pub struct VfioUserPciDevice {
    client: Arc<Mutex<Client>>,
    vfio_wrapper: VfioUserClientWrapper,
    common: VfioCommon,
}

#[derive(Debug)]
pub enum VfioUserPciDeviceError {
    Client(VfioUserError),
    MapRegionGuest(anyhow::Error),
}

#[derive(Copy, Clone)]
enum PciVfioUserSubclass {
    VfioUserSubclass = 0xff,
}

impl PciSubclass for PciVfioUserSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

impl VfioUserPciDevice {
    pub fn new(
        path: &Path,
        mem: &GuestMemoryMmap<AtomicBitmap>,
        msi_interrupt_manager: &Arc<dyn InterruptManager<GroupConfig = MsiIrqGroupConfig>>,
        legacy_interrupt_group: Option<Arc<Box<dyn InterruptSourceGroup>>>,
    ) -> Result<Self, VfioUserPciDeviceError> {
        let client = Client::new(path, mem).map_err(VfioUserPciDeviceError::Client)?;

        // This is used for the BAR and capabilities only
        let configuration = PciConfiguration::new(
            0,
            0,
            0,
            PciClassCode::Other,
            &PciVfioUserSubclass::VfioUserSubclass,
            None,
            PciHeaderType::Device,
            0,
            0,
            None,
        );

        let client = Arc::new(Mutex::new(client));

        let vfio_wrapper = VfioUserClientWrapper {
            client: client.clone(),
        };

        let mut common = VfioCommon {
            mmio_regions: Vec::new(),
            configuration,
            interrupt: Interrupt {
                intx: None,
                msi: None,
                msix: None,
            },
        };

        common.parse_capabilities(msi_interrupt_manager, &vfio_wrapper);
        common
            .initialize_legacy_interrupt(legacy_interrupt_group, &vfio_wrapper)
            .ok();

        let device = Self {
            vfio_wrapper,
            client,
            common,
        };
        Ok(device)
    }
}

impl BusDevice for VfioUserPciDevice {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.read_bar(base, offset, data)
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.write_bar(base, offset, data)
    }
}

#[repr(u32)]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)]
enum Regions {
    Bar0,
    Bar1,
    Bar2,
    Bar3,
    Bar4,
    Bar5,
    Rom,
    Config,
    Vga,
    Migration,
}

struct VfioUserClientWrapper {
    client: Arc<Mutex<Client>>,
}

impl Vfio for VfioUserClientWrapper {
    fn region_read(&self, index: u32, offset: u64, data: &mut [u8]) {
        self.client
            .lock()
            .unwrap()
            .region_read(index, offset, data)
            .ok();
    }

    fn region_write(&self, index: u32, offset: u64, data: &[u8]) {
        self.client
            .lock()
            .unwrap()
            .region_write(index, offset, data)
            .ok();
    }

    fn get_irq_info(&self, irq_index: u32) -> Option<VfioIrq> {
        self.client
            .lock()
            .unwrap()
            .get_irq_info(irq_index)
            .ok()
            .map(|i| VfioIrq {
                index: i.index,
                flags: i.flags,
                count: i.count,
            })
    }

    fn enable_irq(
        &self,
        irq_index: u32,
        event_fds: Vec<&EventFd>,
    ) -> std::result::Result<(), VfioError> {
        info!(
            "Enabling IRQ {:x} number of fds = {:?}",
            irq_index,
            event_fds.len()
        );
        let fds: Vec<i32> = event_fds.iter().map(|e| e.as_raw_fd()).collect();

        self.client
            .lock()
            .unwrap()
            .set_irqs(
                irq_index,
                VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                event_fds.len() as u32,
                &fds,
            )
            .ok();

        Ok(())
    }

    fn disable_irq(&self, irq_index: u32) -> std::result::Result<(), VfioError> {
        info!("Disabling IRQ {:x}", irq_index);
        self.client
            .lock()
            .unwrap()
            .set_irqs(
                irq_index,
                VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
                0,
                0,
                &[],
            )
            .ok();

        Ok(())
    }

    fn unmask_irq(&self, irq_index: u32) -> std::result::Result<(), VfioError> {
        info!("Unmasking IRQ {:x}", irq_index);
        self.client
            .lock()
            .unwrap()
            .set_irqs(
                irq_index,
                VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_UNMASK,
                0,
                1,
                &[],
            )
            .ok();

        Ok(())
    }
}

impl PciDevice for VfioUserPciDevice {
    fn allocate_bars(
        &mut self,
        allocator: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(GuestAddress, GuestUsize, PciBarRegionType)>, PciDeviceError>
    {
        self.common.allocate_bars(allocator, &self.vfio_wrapper)
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.common
            .configuration
            .detect_bar_reprogramming(reg_idx, data)
    }

    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        self.common
            .write_config_register(reg_idx, offset, data, &self.vfio_wrapper)
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        self.common
            .read_config_register(reg_idx, &self.vfio_wrapper)
    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.common.read_bar(base, offset, data, &self.vfio_wrapper)
    }

    fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.common
            .write_bar(base, offset, data, &self.vfio_wrapper)
    }
}

impl VfioUserPciDevice {
    pub fn map_mmio_regions<F>(
        &mut self,
        vm: &Arc<dyn hypervisor::Vm>,
        mem_slot: F,
    ) -> Result<(), VfioUserPciDeviceError>
    where
        F: Fn() -> u32,
    {
        for mmio_region in &mut self.common.mmio_regions {
            let region_flags = self
                .client
                .lock()
                .unwrap()
                .region(mmio_region.index)
                .unwrap()
                .flags;
            let file_offset = self
                .client
                .lock()
                .unwrap()
                .region(mmio_region.index)
                .unwrap()
                .file_offset
                .clone();

            if region_flags & VFIO_REGION_INFO_FLAG_MMAP != 0 {
                let mut prot = 0;
                if region_flags & VFIO_REGION_INFO_FLAG_READ != 0 {
                    prot |= libc::PROT_READ;
                }
                if region_flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
                    prot |= libc::PROT_WRITE;
                }

                let host_addr = unsafe {
                    libc::mmap(
                        null_mut(),
                        mmio_region.length as usize,
                        prot,
                        libc::MAP_SHARED,
                        file_offset.as_ref().unwrap().file().as_raw_fd(),
                        file_offset.as_ref().unwrap().start() as libc::off_t,
                    )
                };

                if host_addr == libc::MAP_FAILED {
                    error!(
                        "Could not mmap regions, error:{}",
                        std::io::Error::last_os_error()
                    );
                    continue;
                }

                let slot = mem_slot();
                let mem_region = vm.make_user_memory_region(
                    slot,
                    mmio_region.start.0,
                    mmio_region.length as u64,
                    host_addr as u64,
                    false,
                    false,
                );

                vm.create_user_memory_region(mem_region)
                    .map_err(|e| VfioUserPciDeviceError::MapRegionGuest(e.into()))?;

                mmio_region.mem_slot = Some(slot);
                mmio_region.host_addr = Some(host_addr as u64);
            }
        }

        Ok(())
    }
}
