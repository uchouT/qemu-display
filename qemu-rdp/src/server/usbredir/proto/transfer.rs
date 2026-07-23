//! usbredir data-transfer routing and RDPEUSB completion dispatch.

use usbredir_proto::{DataKind, DataPacket, Packet};

use super::super::{
    rdpeusb_io::RdpeusbDeviceIo, RdpeusbCompletion, RdpeusbCompletionKind, RedirEvent,
};
use super::{control_transfer, state::ActiveDevice};

pub(super) async fn handle_data(
    active_device: &mut ActiveDevice,
    packet: DataPacket,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<Vec<Packet>> {
    // Every endpoint-based implementation added here must resolve its pipe
    // from the handles installed by a successful SetConfiguration or
    // SetAltSetting transaction.
    if !matches!(&packet.kind, DataKind::Control { .. }) {
        tracing::warn!(
            id = packet.id,
            endpoint = ?packet.endpoint,
            kind = ?packet.kind,
            "Ignored usbredir data transfer whose RDPEUSB translation is not implemented"
        );
        return Ok(Vec::new());
    }

    control_transfer::handle(active_device, packet, io).await
}

pub(super) async fn handle_rdp_event(
    active_device: &mut ActiveDevice,
    event: RedirEvent,
    _io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<Vec<Packet>> {
    // TODO: usbredir <-> rdpeusb translation
    let device_instance_id = active_device.source.device_instance_id.as_str();
    match event {
        RedirEvent::DeviceText(device_text) => {
            tracing::error!(
                hresult = device_text.hresult,
                description_len = device_text.description.len(),
                ?device_instance_id,
                "RDPEUSB device text is awaiting usbredir translation"
            );
        }
        RedirEvent::Completion(RdpeusbCompletion {
            channel_id,
            request_id,
            kind,
        }) => match kind {
            RdpeusbCompletionKind::IoControl(completion) => {
                tracing::error!(
                    channel_id,
                    request_id,
                    hresult = completion.hresult,
                    information = completion.information,
                    output_buffer_len = completion.output_buffer.len(),
                    ?device_instance_id,
                    "RDPEUSB I/O-control completion is awaiting usbredir translation"
                );
            }
            RdpeusbCompletionKind::InternalIoControl(completion) => {
                tracing::error!(
                    channel_id,
                    request_id,
                    hresult = completion.hresult,
                    information = completion.information,
                    output_buffer_len = completion.output_buffer.len(),
                    ?device_instance_id,
                    "RDPEUSB internal-I/O-control completion is awaiting usbredir translation"
                );
            }
            RdpeusbCompletionKind::TransferIn(completion) => {
                tracing::error!(
                    channel_id,
                    request_id,
                    hresult = completion.hresult,
                    usbd_status = completion.ts_urb_result.header.usbd_status,
                    output_buffer_len = completion.output_buffer.len(),
                    ?device_instance_id,
                    "RDPEUSB transfer-in completion is awaiting usbredir translation"
                );
            }
            RdpeusbCompletionKind::TransferOut(completion) => {
                tracing::error!(
                    channel_id,
                    request_id,
                    hresult = completion.hresult,
                    usbd_status = completion.ts_urb_result.header.usbd_status,
                    output_buffer_size = completion.output_buffer_size,
                    ?device_instance_id,
                    "RDPEUSB transfer-out completion is awaiting usbredir translation"
                );
            }
        },
    }
    Ok(Vec::new())
}
