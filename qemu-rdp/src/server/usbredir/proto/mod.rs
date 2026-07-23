use anyhow::{bail, Context as _};
use ironrdp_rdpeusb::io::DeviceAnnounce;
use usbredir_proto::{Caps, Packet};

use super::{rdpeusb_io::RdpeusbDeviceIo, RedirEvent};

mod control_transfer;
mod descriptor;
mod device;
mod request;
mod state;
mod transfer;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// The session task owns only transport framing and I/O. Protocol negotiation,
/// device discovery, packet translation, and request correlation belong here.
pub(super) struct Bridge {
    state: BridgeState,
}

/// Mutually exclusive phases of one usbredir bridge session.
enum BridgeState {
    AwaitingPeerHello(DeviceAnnounce),
    /// Temporary ownership state while device preparation is awaited.
    Preparing,
    Active(Box<state::ActiveDevice>),
}

impl Bridge {
    pub(super) fn new(device: DeviceAnnounce) -> Self {
        Self {
            state: BridgeState::AwaitingPeerHello(device),
        }
    }

    pub(super) async fn handle_usbredir_packet(
        &mut self,
        packet: Packet,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        match packet {
            Packet::Hello { version, caps } => self.handle_peer_hello(version, caps, io).await,
            packet => {
                let device = match &mut self.state {
                    BridgeState::Active(device) => device,
                    BridgeState::AwaitingPeerHello(_) => {
                        bail!("received a usbredir packet before peer Hello")
                    }
                    BridgeState::Preparing => {
                        bail!("received a usbredir packet while the device is being prepared")
                    }
                };

                match packet {
                    Packet::Request(request) => request::handle_control(device, request, io).await,
                    Packet::Data(packet) => transfer::handle_data(device, packet, io).await,
                    Packet::FilterFilter { rules: _ } => todo!(),
                    Packet::FilterReject => todo!(),
                    packet => {
                        tracing::warn!(?packet, "Ignored unexpected usbredir packet from QEMU");
                        Ok(Vec::new())
                    }
                }
            }
        }
    }

    pub(super) async fn handle_rdp_event(
        &mut self,
        event: RedirEvent,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        let device = match &mut self.state {
            BridgeState::Active(device) => device,
            BridgeState::AwaitingPeerHello(announce) => {
                tracing::warn!(
                    device_instance_id = %announce.device_instance_id,
                    "Ignored an RDPEUSB event before peer Hello"
                );
                return Ok(Vec::new());
            }
            BridgeState::Preparing => {
                tracing::warn!("Ignored an RDPEUSB event while the device is being prepared");
                return Ok(Vec::new());
            }
        };
        transfer::handle_rdp_event(device, event, io).await
    }

    async fn handle_peer_hello(
        &mut self,
        version: String,
        caps: Caps,
        io: &mut RdpeusbDeviceIo,
    ) -> anyhow::Result<Vec<Packet>> {
        let previous = std::mem::replace(&mut self.state, BridgeState::Preparing);
        let announce = match previous {
            BridgeState::AwaitingPeerHello(announce) => announce,
            state @ BridgeState::Active(_) => {
                self.state = state;
                bail!("received a second usbredir Hello");
            }
            BridgeState::Preparing => {
                self.state = BridgeState::Preparing;
                bail!("received a usbredir Hello while the device is being prepared");
            }
        };

        tracing::debug!(%version, ?caps, "received QEMU usbredir Hello");
        tracing::debug!(
            device_instance_id = %announce.device_instance_id,
            "preparing RDPEUSB device for usbredir announcement"
        );
        let prepared = device::prepare_device(announce, io)
            .await
            .context("prepare RDPEUSB device for usbredir")?;
        let device = state::ActiveDevice::from_prepared(prepared)
            .context("build canonical usbredir device state")?;
        let device_connect = device.device_connect();

        // usbredir requires endpoint and interface metadata before the device
        // becomes visible to the guest.
        let mut packets = device_metadata_packets(&device);
        packets.push(Packet::device_connect(device_connect));
        self.state = BridgeState::Active(Box::new(device));
        Ok(packets)
    }
}

fn device_metadata_packets(device: &state::ActiveDevice) -> Vec<Packet> {
    let (ep_info, interface_info) = device.metadata();
    vec![
        Packet::ep_info(ep_info),
        Packet::interface_info(interface_info),
    ]
}
