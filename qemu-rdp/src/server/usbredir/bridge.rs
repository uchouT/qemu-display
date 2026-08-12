//! usbredir session semantics on top of the IronRDP USB device facade.

use ironrdp_server::{DeviceAnnounce, DeviceText, RdpUsbDeviceAnnounceInfo, UsbDeviceHandle};
use usbredir_proto::Packet;

use super::data_inflight::{DataCompleted, DataInFlight};

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
    // The usbredir session state — handshake phase, descriptor cache, and the
    // device view last announced to QEMU — belongs here as well.
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
        todo!()
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
}
