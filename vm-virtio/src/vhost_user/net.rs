// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::super::net_util::{build_net_config_space, CtrlVirtio, NetCtrlEpollHandler};
use super::super::Error as CtrlError;
use super::super::{ActivateError, ActivateResult, Queue, VirtioDevice, VirtioDeviceType};
use super::handler::*;
use super::vu_common_ctrl::*;
use super::Error as DeviceError;
use super::{Error, Result};
use crate::VirtioInterrupt;
use arc_swap::ArcSwap;
use libc;
use libc::EFD_NONBLOCK;
use net_util::MacAddr;
use std::cmp;
use std::io::Write;
use std::result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::vec::Vec;
use vhost_rs::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_rs::vhost_user::{Master, VhostUserMaster, VhostUserMasterReqHandler};
use vhost_rs::VhostBackend;
use virtio_bindings::bindings::virtio_net;
use virtio_bindings::bindings::virtio_ring;
use vm_device::{Migratable, MigratableError, Pausable, Snapshotable};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

struct SlaveReqHandler {}
impl VhostUserMasterReqHandler for SlaveReqHandler {}

pub struct Net {
    vhost_user_net: Master,
    kill_evt: Option<EventFd>,
    pause_evt: Option<EventFd>,
    avail_features: u64,
    acked_features: u64,
    backend_features: u64,
    config_space: Vec<u8>,
    queue_sizes: Vec<u16>,
    queue_evts: Option<Vec<EventFd>>,
    interrupt_cb: Option<Arc<VirtioInterrupt>>,
    epoll_thread: Option<thread::JoinHandle<result::Result<(), DeviceError>>>,
    ctrl_queue_epoll_thread: Option<thread::JoinHandle<result::Result<(), CtrlError>>>,
    paused: Arc<AtomicBool>,
}

impl Net {
    /// Create a new vhost-user-net device
    pub fn new(mac_addr: MacAddr, vu_cfg: VhostUserConfig) -> Result<Net> {
        let mut vhost_user_net = Master::connect(&vu_cfg.sock, vu_cfg.num_queues as u64)
            .map_err(Error::VhostUserCreateMaster)?;

        // Filling device and vring features VMM supports.
        let mut avail_features = 1 << virtio_net::VIRTIO_NET_F_GUEST_CSUM
            | 1 << virtio_net::VIRTIO_NET_F_CSUM
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_TSO4
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_TSO6
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_ECN
            | 1 << virtio_net::VIRTIO_NET_F_GUEST_UFO
            | 1 << virtio_net::VIRTIO_NET_F_HOST_TSO4
            | 1 << virtio_net::VIRTIO_NET_F_HOST_TSO6
            | 1 << virtio_net::VIRTIO_NET_F_HOST_ECN
            | 1 << virtio_net::VIRTIO_NET_F_HOST_UFO
            | 1 << virtio_net::VIRTIO_NET_F_MRG_RXBUF
            | 1 << virtio_net::VIRTIO_F_NOTIFY_ON_EMPTY
            | 1 << virtio_net::VIRTIO_F_VERSION_1
            | 1 << virtio_ring::VIRTIO_RING_F_EVENT_IDX
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

        vhost_user_net
            .set_owner()
            .map_err(Error::VhostUserSetOwner)?;

        // Get features from backend, do negotiation to get a feature collection which
        // both VMM and backend support.
        let backend_features = vhost_user_net
            .get_features()
            .map_err(Error::VhostUserGetFeatures)?;
        avail_features &= backend_features;
        // Set features back is required by the vhost crate mechanism, since the
        // later vhost call will check if features is filled in master before execution.
        vhost_user_net
            .set_features(avail_features)
            .map_err(Error::VhostUserSetFeatures)?;

        let mut acked_features = 0;
        if avail_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0 {
            acked_features |= VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
            let mut protocol_features = vhost_user_net
                .get_protocol_features()
                .map_err(Error::VhostUserGetProtocolFeatures)?;
            protocol_features &= VhostUserProtocolFeatures::MQ;
            vhost_user_net
                .set_protocol_features(protocol_features)
                .map_err(Error::VhostUserSetProtocolFeatures)?;
        } else {
            return Err(Error::VhostUserProtocolNotSupport);
        }

        avail_features |= 1 << virtio_net::VIRTIO_NET_F_CTRL_VQ;
        let queue_num = vu_cfg.num_queues + 1;

        let config_space = build_net_config_space(mac_addr, &mut avail_features);

        // Send set_vring_base here, since it could tell backends, like OVS + DPDK,
        // how many virt queues to be handled, which backend required to know at early stage.
        for i in 0..vu_cfg.num_queues {
            vhost_user_net
                .set_vring_base(i, 0)
                .map_err(Error::VhostUserSetVringBase)?;
        }

        Ok(Net {
            vhost_user_net,
            kill_evt: None,
            pause_evt: None,
            avail_features,
            acked_features,
            backend_features,
            config_space,
            queue_sizes: vec![vu_cfg.queue_size; queue_num],
            queue_evts: None,
            interrupt_cb: None,
            epoll_thread: None,
            ctrl_queue_epoll_thread: None,
            paused: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            if let Err(e) = kill_evt.write(1) {
                error!("failed to kill vhost-user-net: {:?}", e);
            }
        }
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        VirtioDeviceType::TYPE_NET as u32
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            0 => self.avail_features as u32,
            1 => (self.avail_features >> 32) as u32,
            _ => {
                warn!("Received request for unknown features page: {}", page);
                0u32
            }
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => {
                warn!("Cannot acknowledge unknown features page: {}", page);
                0u64
            }
        };

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("Received acknowledge request for unknown feature: {:x}", v);
            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_len = self.config_space.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&self.config_space[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_len = self.config_space.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            return;
        }
        let (_, right) = self.config_space.split_at_mut(offset as usize);
        right.copy_from_slice(&data[..]);
    }

    fn activate(
        &mut self,
        mem: Arc<ArcSwap<GuestMemoryMmap>>,
        interrupt_cb: Arc<VirtioInterrupt>,
        mut queues: Vec<Queue>,
        mut queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        if queues.len() != self.queue_sizes.len() || queue_evts.len() != self.queue_sizes.len() {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                self.queue_sizes.len(),
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        let (self_kill_evt, kill_evt) = EventFd::new(EFD_NONBLOCK)
            .and_then(|e| Ok((e.try_clone()?, e)))
            .map_err(|e| {
                error!("failed creating kill EventFd pair: {}", e);
                ActivateError::BadActivate
            })?;
        self.kill_evt = Some(self_kill_evt);

        let (self_pause_evt, pause_evt) = EventFd::new(EFD_NONBLOCK)
            .and_then(|e| Ok((e.try_clone()?, e)))
            .map_err(|e| {
                error!("failed creating pause EventFd pair: {}", e);
                ActivateError::BadActivate
            })?;
        self.pause_evt = Some(self_pause_evt);

        // Save the interrupt EventFD as we need to return it on reset
        // but clone it to pass into the thread.
        self.interrupt_cb = Some(interrupt_cb.clone());

        let mut tmp_queue_evts: Vec<EventFd> = Vec::new();
        for queue_evt in queue_evts.iter() {
            // Save the queue EventFD as we need to return it on reset
            // but clone it to pass into the thread.
            tmp_queue_evts.push(queue_evt.try_clone().map_err(|e| {
                error!("failed to clone queue EventFd: {}", e);
                ActivateError::BadActivate
            })?);
        }
        self.queue_evts = Some(tmp_queue_evts);

        let queue_num = queue_evts.len();

        if (self.acked_features & 1 << virtio_net::VIRTIO_NET_F_CTRL_VQ) != 0 && queue_num % 2 != 0
        {
            let cvq_queue = queues.remove(queue_num - 1);
            let cvq_queue_evt = queue_evts.remove(queue_num - 1);

            let mut ctrl_handler = NetCtrlEpollHandler {
                mem: mem.clone(),
                kill_evt: kill_evt.try_clone().unwrap(),
                pause_evt: pause_evt.try_clone().unwrap(),
                ctrl_q: CtrlVirtio::new(cvq_queue, cvq_queue_evt),
                epoll_fd: 0,
            };

            let paused = self.paused.clone();
            thread::Builder::new()
                .name("virtio_net".to_string())
                .spawn(move || ctrl_handler.run_ctrl(paused))
                .map(|thread| self.ctrl_queue_epoll_thread = Some(thread))
                .map_err(|e| {
                    error!("failed to clone queue EventFd: {}", e);
                    ActivateError::BadActivate
                })?;
        }

        let vu_interrupt_list = setup_vhost_user(
            &mut self.vhost_user_net,
            mem.load().as_ref(),
            queues,
            queue_evts,
            self.acked_features & self.backend_features,
        )
        .map_err(ActivateError::VhostUserNetSetup)?;

        let mut handler = VhostUserEpollHandler::<SlaveReqHandler>::new(VhostUserEpollConfig {
            interrupt_cb,
            kill_evt,
            pause_evt,
            vu_interrupt_list,
            slave_req_handler: None,
        });

        let paused = self.paused.clone();
        thread::Builder::new()
            .name("vhost_user_net".to_string())
            .spawn(move || handler.run(paused))
            .map(|thread| self.epoll_thread = Some(thread))
            .map_err(|e| {
                error!("failed to clone queue EventFd: {}", e);
                ActivateError::BadActivate
            })?;

        Ok(())
    }

    fn reset(&mut self) -> Option<(Arc<VirtioInterrupt>, Vec<EventFd>)> {
        // We first must resume the virtio thread if it was paused.
        if self.pause_evt.take().is_some() {
            self.resume().ok()?;
        }

        if let Err(e) = reset_vhost_user(&mut self.vhost_user_net, self.queue_sizes.len()) {
            error!("Failed to reset vhost-user daemon: {:?}", e);
            return None;
        }

        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        // Return the interrupt and queue EventFDs
        Some((
            self.interrupt_cb.take().unwrap(),
            self.queue_evts.take().unwrap(),
        ))
    }
}

virtio_pausable!(Net, true);
impl Snapshotable for Net {}
impl Migratable for Net {}
