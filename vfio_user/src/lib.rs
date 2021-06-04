// Copyright © 2021 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::ffi::CString;
use std::io::{IoSlice, Read, Write};
use std::num::Wrapping;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::prelude::RawFd;
use std::path::Path;
use vfio_bindings::bindings::vfio::*;
use vm_memory::bitmap::AtomicBitmap;
use vm_memory::{ByteValued, FileOffset, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate log;

#[allow(dead_code)]
#[repr(u16)]
#[derive(Clone, Copy, Debug)]
enum Command {
    Unknown = 0,
    Version = 1,
    DmaMap = 2,
    DmaUnmap = 3,
    DeviceGetInfo = 4,
    DeviceGetRegionInfo = 5,
    GetRegionIoFds = 6,
    GetIrqInfo = 7,
    SetIrqs = 8,
    RegionRead = 9,
    RegionWrite = 10,
    DmaRead = 11,
    DmaWrite = 12,
    DeviceReset = 13,
    UserDirtyPages,
}

impl Default for Command {
    fn default() -> Self {
        Command::Unknown
    }
}

#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum HeaderFlags {
    Command = 0,
    Reply = 1,
    NoReply = 1 << 4,
    Error = 1 << 5,
}

impl Default for HeaderFlags {
    fn default() -> Self {
        HeaderFlags::Command
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct Header {
    message_id: u16,
    command: Command,
    message_size: u32,
    flags: u32,
    error: u32,
}

unsafe impl ByteValued for Header {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct Version {
    header: Header,
    major: u16,
    minor: u16,
}
unsafe impl ByteValued for Version {}

#[derive(Serialize, Deserialize, Debug)]
struct MigrationCapabilities {
    pgsize: u32,
}

const fn default_max_msg_fds() -> u32 {
    1
}

const fn default_max_data_xfer_size() -> u32 {
    1048576
}

const fn default_migration_capabilities() -> MigrationCapabilities {
    MigrationCapabilities { pgsize: 4096 }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum DmaMapFlags {
    Unknown = 0,
    ReadOnly = 1,
    WriteOnly = 2,
    ReadWrite = 3,
}

impl Default for DmaMapFlags {
    fn default() -> Self {
        Self::Unknown
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct DmaMap {
    header: Header,
    argsz: u32,
    flags: DmaMapFlags,
    offset: u64,
    address: u64,
    size: u64,
}

unsafe impl ByteValued for DmaMap {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct DeviceGetInfo {
    header: Header,
    argsz: u32,
    flags: u32,
    num_regions: u32,
    num_irqs: u32,
}

unsafe impl ByteValued for DeviceGetInfo {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct DeviceGetRegionInfo {
    header: Header,
    region_info: vfio_region_info,
}

unsafe impl ByteValued for DeviceGetRegionInfo {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct RegionAccess {
    header: Header,
    offset: u64,
    region: u32,
    count: u32,
}

unsafe impl ByteValued for RegionAccess {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct GetIrqInfo {
    header: Header,
    argsz: u32,
    flags: u32,
    index: u32,
    count: u32,
}

unsafe impl ByteValued for GetIrqInfo {}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct SetIrqs {
    header: Header,
    argsz: u32,
    flags: u32,
    index: u32,
    start: u32,
    count: u32,
}

unsafe impl ByteValued for SetIrqs {}

#[derive(Serialize, Deserialize, Debug)]
struct Capabilities {
    #[serde(default = "default_max_msg_fds")]
    max_msg_fds: u32,
    #[serde(default = "default_max_data_xfer_size")]
    max_data_xfer_size: u32,
    #[serde(default = "default_migration_capabilities")]
    migration: MigrationCapabilities,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            max_msg_fds: default_max_msg_fds(),
            max_data_xfer_size: default_max_data_xfer_size(),
            migration: default_migration_capabilities(),
        }
    }
}

pub struct Client {
    stream: UnixStream,
    next_message_id: Wrapping<u16>,
    num_irqs: u32,
    resettable: bool,
    regions: Vec<Region>,
}

#[derive(Debug)]
pub struct Region {
    pub flags: u32,
    pub index: u32,
    pub size: u64,
    pub file_offset: Option<FileOffset>,
}

#[derive(Debug)]
pub struct IrqInfo {
    pub index: u32,
    pub flags: u32,
    pub count: u32,
}

#[derive(Debug)]
pub enum Error {
    Connect(std::io::Error),
    SerializeCapabilites(serde_json::Error),
    DeserializeCapabilites(serde_json::Error),
    StreamWrite(std::io::Error),
    StreamRead(std::io::Error),
    SendWithFd(vmm_sys_util::errno::Error),
    ReceiveWithFd(vmm_sys_util::errno::Error),
    NotPciDevice,
}

impl Client {
    pub fn new(path: &Path, mem: &GuestMemoryMmap<AtomicBitmap>) -> Result<Client, Error> {
        let stream = UnixStream::connect(path).map_err(Error::Connect)?;

        let mut client = Client {
            next_message_id: Wrapping(0),
            stream,
            num_irqs: 0,
            resettable: false,
            regions: Vec::new(),
        };

        client.negotiate_version()?;

        for region in mem.iter() {
            let (fd, offset) = match region.file_offset() {
                Some(_file_offset) => (_file_offset.file().as_raw_fd(), _file_offset.start()),
                None => continue,
            };

            client.dma_map(offset, region.start_addr().0, region.len(), fd)?;
        }

        client.regions = client.get_regions()?;

        Ok(client)
    }

    fn negotiate_version(&mut self) -> Result<(), Error> {
        let caps = Capabilities::default();

        let version_data = serde_json::to_string(&caps).map_err(Error::SerializeCapabilites)?;

        let version = Version {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::Version,
                flags: HeaderFlags::Command as u32,
                message_size: (std::mem::size_of::<Version>() + version_data.len() + 1) as u32,
                ..Default::default()
            },
            major: 0,
            minor: 1,
        };

        let version_data = CString::new(version_data.as_bytes()).unwrap();
        let mut bufs = Vec::new();
        bufs.push(IoSlice::new(&version.as_slice()));
        bufs.push(IoSlice::new(version_data.as_bytes_with_nul()));

        // TODO: Use write_all_vectored() when ready
        let _ = self
            .stream
            .write_vectored(&bufs)
            .map_err(Error::StreamWrite)?;

        info!(
            "Sent client version information: major = {} minor = {} capabilities = {:?}",
            version.major, version.minor, &caps
        );

        self.next_message_id += Wrapping(1);

        let mut server_version: Version = Version::default();
        self.stream
            .read_exact(server_version.as_mut_slice())
            .map_err(Error::StreamRead)?;

        let mut server_version_data = Vec::new();
        server_version_data.resize(
            server_version.header.message_size as usize - std::mem::size_of::<Version>(),
            0,
        );
        self.stream
            .read_exact(server_version_data.as_mut_slice())
            .map_err(Error::StreamRead)?;

        let server_caps: Capabilities =
            serde_json::from_slice(&server_version_data[0..server_version_data.len() - 1])
                .map_err(Error::DeserializeCapabilites)?;

        info!(
            "Received server version information: major = {} minor = {} capabilities = {:?}",
            server_version.major, server_version.minor, &server_caps
        );

        Ok(())
    }

    fn dma_map(&mut self, offset: u64, address: u64, size: u64, fd: RawFd) -> Result<(), Error> {
        let dma_map = DmaMap {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::DmaMap,
                flags: HeaderFlags::Command as u32,
                message_size: std::mem::size_of::<DmaMap>() as u32,
                ..Default::default()
            },
            argsz: (std::mem::size_of::<DmaMap>() - std::mem::size_of::<Header>()) as u32,
            flags: DmaMapFlags::ReadWrite,
            offset,
            address,
            size,
        };

        self.next_message_id += Wrapping(1);

        info!("Sending DMA map command: {:?} and fd: {:?}", &dma_map, &fd);

        self.stream
            .send_with_fd(dma_map.as_slice(), fd)
            .map_err(Error::SendWithFd)?;

        let mut reply = Header::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;

        info!("Received reply: {:?}", reply);

        Ok(())
    }

    fn get_regions(&mut self) -> Result<Vec<Region>, Error> {
        let get_info = DeviceGetInfo {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::DeviceGetInfo,
                flags: HeaderFlags::Command as u32,
                message_size: std::mem::size_of::<DeviceGetInfo>() as u32,
                ..Default::default()
            },
            argsz: std::mem::size_of::<DeviceGetInfo>() as u32,
            ..Default::default()
        };
        self.next_message_id += Wrapping(1);

        self.stream
            .write_all(&get_info.as_slice())
            .map_err(Error::StreamWrite)?;

        let mut reply = DeviceGetInfo::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;

        self.num_irqs = reply.num_irqs;

        if reply.flags & VFIO_DEVICE_FLAGS_PCI != VFIO_DEVICE_FLAGS_PCI {
            return Err(Error::NotPciDevice);
        }

        self.resettable = reply.flags & VFIO_DEVICE_FLAGS_RESET != VFIO_DEVICE_FLAGS_RESET;

        info!("Received reply: {:?}", reply);

        let num_regions = reply.num_regions;
        let mut regions = Vec::new();
        for index in 0..num_regions {
            let get_region_info = DeviceGetRegionInfo {
                header: Header {
                    message_id: self.next_message_id.0,
                    command: Command::DeviceGetRegionInfo,
                    flags: HeaderFlags::Command as u32,
                    message_size: std::mem::size_of::<DeviceGetRegionInfo>() as u32,
                    ..Default::default()
                },
                region_info: vfio_region_info {
                    argsz: 1024, // Arbitrary max size
                    index,
                    ..Default::default()
                },
            };
            self.next_message_id += Wrapping(1);

            self.stream
                .write_all(&get_region_info.as_slice())
                .map_err(Error::StreamWrite)?;

            let mut reply = DeviceGetRegionInfo::default();
            let (_, fd) = self
                .stream
                .recv_with_fd(reply.as_mut_slice())
                .map_err(Error::ReceiveWithFd)?;

            regions.push(Region {
                flags: reply.region_info.flags,
                index: reply.region_info.index,
                size: reply.region_info.size,
                file_offset: fd.map(|fd| FileOffset::new(fd, reply.region_info.offset)),
            });

            // TODO: Handle region with capabilities
            let mut _cap_data = Vec::with_capacity(
                reply.header.message_size as usize - std::mem::size_of::<DeviceGetRegionInfo>(),
            );
            _cap_data.resize(_cap_data.capacity(), 0u8);
            self.stream
                .read_exact(_cap_data.as_mut_slice())
                .map_err(Error::StreamRead)?;
        }

        info!("Received regions: {:?}", regions);
        Ok(regions)
    }

    pub fn region_read(&mut self, region: u32, offset: u64, data: &mut [u8]) -> Result<(), Error> {
        let region_read = RegionAccess {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::RegionRead,
                flags: HeaderFlags::Command as u32,
                message_size: std::mem::size_of::<RegionAccess>() as u32,
                ..Default::default()
            },
            offset,
            count: data.len() as u32,
            region,
        };
        self.next_message_id += Wrapping(1);
        info!("Region read: {:?}", region_read);
        self.stream
            .write_all(&region_read.as_slice())
            .map_err(Error::StreamWrite)?;

        let mut reply = RegionAccess::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;
        info!("Reply: {:?}", reply);
        self.stream.read_exact(data).map_err(Error::StreamRead)?;
        Ok(())
    }

    pub fn region_write(&mut self, region: u32, offset: u64, data: &[u8]) -> Result<(), Error> {
        let region_write = RegionAccess {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::RegionWrite,
                flags: HeaderFlags::Command as u32,
                message_size: (std::mem::size_of::<RegionAccess>() + data.len()) as u32,
                ..Default::default()
            },
            offset,
            count: data.len() as u32,
            region,
        };
        self.next_message_id += Wrapping(1);
        info!("Region write: {:?}", region_write);
        let mut bufs = Vec::new();
        bufs.push(IoSlice::new(&region_write.as_slice()));
        bufs.push(IoSlice::new(data));

        // TODO: Use write_all_vectored() when ready
        let _ = self
            .stream
            .write_vectored(&bufs)
            .map_err(Error::StreamWrite)?;

        let mut reply = RegionAccess::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;

        info!("Reply: {:?}", reply);
        Ok(())
    }

    pub fn get_irq_info(&mut self, index: u32) -> Result<IrqInfo, Error> {
        let get_irq_info = GetIrqInfo {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::GetIrqInfo,
                flags: HeaderFlags::Command as u32,
                message_size: std::mem::size_of::<GetIrqInfo>() as u32,
                ..Default::default()
            },
            argsz: (std::mem::size_of::<GetIrqInfo>() - std::mem::size_of::<Header>()) as u32,
            flags: 0,
            index,
            count: 0,
        };
        self.next_message_id += Wrapping(1);

        info!("Get IRQ info: {:?}", get_irq_info);
        self.stream
            .write_all(&get_irq_info.as_slice())
            .map_err(Error::StreamWrite)?;

        let mut reply = GetIrqInfo::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;
        info!("Received reply: {:?}", reply);

        return Ok(IrqInfo {
            index: reply.index,
            flags: reply.flags,
            count: reply.count,
        });
    }

    pub fn set_irqs(
        &mut self,
        index: u32,
        flags: u32,
        start: u32,
        count: u32,
        fds: &[RawFd],
    ) -> Result<(), Error> {
        let set_irqs = SetIrqs {
            header: Header {
                message_id: self.next_message_id.0,
                command: Command::SetIrqs,
                flags: HeaderFlags::Command as u32,
                message_size: std::mem::size_of::<SetIrqs>() as u32,
                ..Default::default()
            },
            argsz: (std::mem::size_of::<SetIrqs>() - std::mem::size_of::<Header>()) as u32,
            flags,
            start,
            index,
            count,
        };

        self.next_message_id += Wrapping(1);

        info!("Sending SET_IRQs command: {:?}", &set_irqs);

        self.stream
            .send_with_fds(&[set_irqs.as_slice()], fds)
            .map_err(Error::SendWithFd)?;

        let mut reply = Header::default();
        self.stream
            .read_exact(reply.as_mut_slice())
            .map_err(Error::StreamRead)?;

        info!("Received reply: {:?}", reply);

        Ok(())
    }

    pub fn regions(&self) -> &Vec<Region> {
        &self.regions
    }

    pub fn region(&self, region_index: u32) -> Option<&Region> {
        for region in &self.regions {
            if region.index == region_index {
                return Some(region);
            }
        }

        None
    }
}
