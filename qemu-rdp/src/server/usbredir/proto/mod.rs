use ironrdp_server::UsbDeviceHandle;
use usbredir_proto::Packet;

use super::RedirEvent;

pub(crate) mod descriptor;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// Packet translation and request correlation will be implemented here. The
/// session task only owns transport framing and forwards typed messages.
pub(super) struct Bridge {
    rdp_handle: UsbDeviceHandle,
    configuration: u8,
}

impl Bridge {
    pub(super) fn new(rdp_handle: UsbDeviceHandle, configuration: u8) -> Self {
        Self {
            rdp_handle,
            configuration,
        }
    }

    pub(super) async fn handle_usbredir_packet(&mut self, packet: Packet) -> anyhow::Result<()> {
        // TODO: usbredir <-> rdpeusb translation
        tracing::debug!(?packet, "usbredir packet is awaiting RDPEUSB translation");
        Ok(())
    }

    pub(super) fn handle_rdp_event(&mut self, event: RedirEvent) -> anyhow::Result<Option<Packet>> {
        // TODO: usbredir <-> rdpeusb translation
        tracing::debug!(?event, "RDPEUSB event is awaiting usbredir translation");
        Ok(None)
    }
}
