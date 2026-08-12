//! usbredir session semantics on top of the IronRDP USB device facade.

use anyhow::{ensure, Context as _};
use ironrdp_rdpeusb::pdu::sink::DeviceSpeed;
use ironrdp_server::{DeviceAnnounce, DeviceText, RdpUsbDeviceAnnounceInfo, UsbDeviceHandle};
use usbredir_proto::{Caps, Packet, Speed};

use super::data_inflight::{DataCompleted, DataInFlight};
use super::device::ActiveDevice;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// The session task owns only transport framing and I/O. Protocol negotiation,
/// device state, packet translation, and request correlation belong here.
///
/// Handlers append their responses to a caller-owned buffer instead of
/// returning one: a data-path response then costs no allocation, and every
/// handler has the same shape.
pub(super) struct Bridge {
    usb: UsbDeviceHandle,
    inflight: DataInFlight,
    /// The device as RDPEUSB announced it, before any usbredir negotiation.
    announce: DeviceAnnounce,
    state: State,
}

/// Mutually exclusive phases of one usbredir bridge session.
///
/// Device preparation needs no phase of its own: packets are dispatched one at
/// a time, so no other packet can be observed while the peer Hello is still
/// being answered.
enum State {
    AwaitingPeerHello,
    Active(Box<ActiveDevice>),
}

impl Bridge {
    pub(super) fn new(info: RdpUsbDeviceAnnounceInfo) -> Self {
        let RdpUsbDeviceAnnounceInfo {
            announce,
            usb_handle,
        } = info;

        Self {
            usb: usb_handle,
            inflight: DataInFlight::new(),
            announce,
            state: State::AwaitingPeerHello,
        }
    }

    /// Waits for the next in-flight data request to end.
    ///
    /// Cancel-safe, so the session task can select on it: a request only
    /// leaves [`DataInFlight`] once its future has yielded, and nothing is
    /// awaited after that point.
    pub(super) async fn next_completion(&mut self) -> anyhow::Result<DataCompleted> {
        self.inflight.completed().await
    }

    /// Translates one packet received from QEMU.
    pub(super) async fn handle_usbredir_packet(
        &mut self,
        packet: Packet,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        match packet {
            Packet::Hello { version, caps } => self.handle_peer_hello(&version, caps, out).await,
            packet => {
                let State::Active(device) = &mut self.state else {
                    anyhow::bail!("received a usbredir packet before peer Hello");
                };

                let _ = (device, packet, out);
                todo!("usbredir request and data packet dispatch")
            }
        }
    }

    /// Turns a finished USB request into the response its packet ID owes QEMU.
    pub(super) async fn handle_completion(
        &mut self,
        completed: DataCompleted,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        todo!()
    }

    /// Handles device text reported by the RDP client.
    pub(super) async fn handle_device_text(
        &mut self,
        text: DeviceText,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        todo!()
    }

    /// Answers the peer Hello by making the redirected device visible to QEMU.
    async fn handle_peer_hello(
        &mut self,
        version: &str,
        caps: Caps,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(self.state, State::AwaitingPeerHello),
            "received a second usbredir Hello"
        );

        tracing::debug!(version, ?caps, "received QEMU usbredir Hello");
        tracing::debug!(
            device_instance_id = %self.announce.device_instance_id,
            "preparing RDPEUSB device for usbredir announcement"
        );

        let device = ActiveDevice::prepare(&self.usb, self.device_speed())
            .await
            .context("prepare rdpeusb device for usbredir")?;

        // usbredir requires endpoint and interface metadata to precede the
        // device becoming visible to the guest.
        let (ep_info, interface_info) = device.metadata();
        out.push(Packet::ep_info(ep_info));
        out.push(Packet::interface_info(interface_info));
        out.push(Packet::device_connect(device.device_connect()));

        self.state = State::Active(Box::new(device));
        Ok(())
    }

    /// Ref: MS-RDPEUSB `UsbDeviceCaps.DeviceSpeed` and usbredir v0.7
    /// `usb_redir_device_connect`.
    ///
    /// RDPEUSB only distinguishes full from high speed, and `bcdUSB` alone does
    /// not prove SuperSpeed, so no other speed can be reported honestly.
    fn device_speed(&self) -> Speed {
        match self.announce.usb_device_caps.device_speed {
            DeviceSpeed::HIGH_SPEED => Speed::High,
            DeviceSpeed::FULL_SPEED => Speed::Full,
            _ => Speed::Unknown,
        }
    }
}
