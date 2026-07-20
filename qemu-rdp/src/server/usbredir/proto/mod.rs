use anyhow::{ensure, Context as _};
use ironrdp_rdpeusb::io::DeviceAnnounce;
use usbredir_proto::{EpInfoData, InterfaceInfoData, Packet};

use super::{rdpeusb_io::RdpeusbDeviceIo, RedirEvent};

mod control_transfer;
mod descriptor;
mod device;
mod request;
mod transfer;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// The session task owns only transport framing and I/O. Protocol negotiation,
/// device discovery, packet translation, and request correlation belong here.
pub(super) struct Bridge {
    pending_device: Option<DeviceAnnounce>,
    peer_hello_received: bool,
    device: Option<ActiveDevice>,
}

struct ActiveDevice {
    source: DeviceAnnounce,
    configuration: u8,
    interface_info: InterfaceInfoData,
    ep_info: EpInfoData,
}

impl Bridge {
    pub(super) fn new(device: DeviceAnnounce) -> Self {
        Self {
            pending_device: Some(device),
            peer_hello_received: false,
            device: None,
        }
    }

    pub(super) async fn handle_usbredir_packet(
        &mut self,
        packet: Packet,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        match packet {
            Packet::Hello { version, caps } => {
                ensure!(
                    !self.peer_hello_received,
                    "received a second usbredir Hello"
                );
                self.peer_hello_received = true;
                tracing::debug!(%version, ?caps, "received QEMU usbredir Hello");
                self.prepare_device_if_negotiated(io).await
            }
            Packet::Request(request) => {
                self.ensure_device_ready()?;
                request::handle(self, request, io).await
            }
            Packet::Data(packet) => {
                self.ensure_device_ready()?;
                transfer::handle_data(packet, io).await
            }
            Packet::FilterFilter { rules } => todo!(),
            Packet::FilterReject => todo!(),
            packet => {
                ensure!(
                    self.peer_hello_received,
                    "received a usbredir packet before peer Hello"
                );
                tracing::warn!(?packet, "Ignored unexpected usbredir packet from QEMU");
                Ok(Vec::new())
            }
        }
    }

    pub(super) async fn handle_rdp_event(
        &mut self,
        event: RedirEvent,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        transfer::handle_rdp_event(self, event, io).await
    }

    fn ensure_device_ready(&self) -> anyhow::Result<()> {
        ensure!(
            self.peer_hello_received,
            "received a usbredir packet before peer Hello"
        );
        ensure!(
            self.pending_device.is_none(),
            "received a usbredir device packet before device announcement"
        );
        ensure!(
            self.device.is_some(),
            "received a usbredir device packet before device preparation completed"
        );
        Ok(())
    }

    async fn prepare_device_if_negotiated(
        &mut self,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        if !self.peer_hello_received {
            return Ok(Vec::new());
        }

        let Some(announce) = self.pending_device.take() else {
            return Ok(Vec::new());
        };

        tracing::debug!(
            device_instance_id = %announce.device_instance_id,
            "preparing RDPEUSB device for usbredir announcement"
        );
        let device = device::prepare_device(announce, io)
            .await
            .context("prepare RDPEUSB device for usbredir")?;
        let device::DeviceAnnouncement {
            device_connect,
            interface_info,
            ep_info,
        } = device.announcement;
        self.device = Some(ActiveDevice {
            source: device.source,
            configuration: device.configuration,
            interface_info,
            ep_info,
        });

        // usbredir requires endpoint and interface metadata before the device
        // becomes visible to the guest.
        let mut packets = self.device_metadata_packets()?;
        packets.push(Packet::device_connect(device_connect));
        Ok(packets)
    }

    fn active_device(&self) -> anyhow::Result<&ActiveDevice> {
        self.device
            .as_ref()
            .context("usbredir device is not prepared")
    }

    fn active_device_mut(&mut self) -> anyhow::Result<&mut ActiveDevice> {
        self.device
            .as_mut()
            .context("usbredir device is not prepared")
    }

    fn device_metadata_packets(&self) -> anyhow::Result<Vec<Packet>> {
        let device = self.active_device()?;
        Ok(vec![
            Packet::ep_info(device.ep_info.clone()),
            Packet::interface_info(device.interface_info.clone()),
        ])
    }
}
