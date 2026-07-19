use anyhow::{ensure, Context as _};
use ironrdp_rdpeusb::io::DeviceAnnounce;
use usbredir_proto::Packet;

use super::{rdpeusb_io::RdpeusbDeviceIo, RedirEvent};

mod descriptor;
mod device;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// The session task owns only transport framing and I/O. Protocol negotiation,
/// device discovery, packet translation, and request correlation belong here.
pub(super) struct Bridge {
    pending_device: Option<DeviceAnnounce>,
    peer_hello_received: bool,
    device: Option<DeviceAnnounce>,
    configuration: Option<u8>,
}

impl Bridge {
    pub(super) fn new(device: DeviceAnnounce) -> Self {
        Self {
            pending_device: Some(device),
            peer_hello_received: false,
            device: None,
            configuration: None,
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
            }
            packet => {
                ensure!(
                    self.peer_hello_received,
                    "received a usbredir packet before peer Hello"
                );
                ensure!(
                    self.pending_device.is_none(),
                    "received a usbredir device packet before device announcement"
                );

                // TODO: usbredir <-> rdpeusb translation
                tracing::error!(
                    ?packet,
                    device_instance_id = ?self.device.as_ref().map(|device| device.device_instance_id.as_str()),
                    configuration = ?self.configuration,
                    "usbredir packet is awaiting RDPEUSB translation"
                );
            }
        }

        self.prepare_device_if_negotiated(io).await
    }

    pub(super) fn handle_rdp_event(&mut self, event: RedirEvent) -> anyhow::Result<Option<Packet>> {
        // TODO: usbredir <-> rdpeusb translation
        tracing::error!(?event, "RDPEUSB event is awaiting usbredir translation");
        Ok(None)
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
        self.device = Some(device.source);
        self.configuration = Some(device.configuration);

        // usbredir requires endpoint and interface metadata before the device
        // becomes visible to the guest.
        let announcement = device.announcement;
        Ok(vec![
            Packet::ep_info(announcement.ep_info),
            Packet::interface_info(announcement.interface_info),
            Packet::device_connect(announcement.device_connect),
        ])
    }
}
