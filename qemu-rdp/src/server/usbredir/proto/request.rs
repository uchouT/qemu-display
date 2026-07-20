//! Translation of usbredir request packets into synchronous RDPEUSB operations.

use usbredir_proto::{Packet, RequestKind, RequestPacket, Status};

use super::super::rdpeusb_io::RdpeusbDeviceIo;
use super::{device, Bridge};

pub(super) async fn handle(
    bridge: &mut Bridge,
    request: RequestPacket,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<Vec<Packet>> {
    let RequestPacket { id, kind } = request;

    match kind {
        RequestKind::Reset => {
            // usbredir has no Reset response. The physical device is
            // already in a stable, configured RDPEUSB state; an actual
            // port reset requires IO_CONTROL support in RdpeusbDeviceIo.
            tracing::debug!(
                id,
                "Received usbredir Reset while RDPEUSB port reset is unavailable"
            );
            Ok(Vec::new())
        }
        RequestKind::GetConfiguration => {
            let fallback = bridge.active_device()?.configuration;
            let packet = match device::get_configuration(io).await {
                Ok(configuration) => {
                    bridge.active_device_mut()?.configuration = configuration;
                    Packet::configuration_status(id, Status::Success, configuration)
                }
                Err(error) => {
                    tracing::warn!(id, %error, "RDPEUSB GET_CONFIGURATION failed");
                    Packet::configuration_status(id, Status::IoError, fallback)
                }
            };
            Ok(vec![packet])
        }
        RequestKind::SetConfiguration { configuration } => {
            let fallback = bridge.active_device()?.configuration;
            match device::get_configuration(io).await {
                Ok(active) if active == configuration => {
                    bridge.active_device_mut()?.configuration = active;
                    let mut packets = bridge.device_metadata_packets()?;
                    packets.push(Packet::configuration_status(id, Status::Success, active));
                    Ok(packets)
                }
                Ok(active) => {
                    tracing::warn!(
                        id,
                        requested = configuration,
                        active,
                        "Rejected usbredir configuration change without RDPEUSB handle rebinding"
                    );
                    Ok(vec![Packet::configuration_status(
                        id,
                        Status::Inval,
                        active,
                    )])
                }
                Err(error) => {
                    tracing::warn!(id, %error, "RDPEUSB GET_CONFIGURATION failed");
                    Ok(vec![Packet::configuration_status(
                        id,
                        Status::IoError,
                        fallback,
                    )])
                }
            }
        }
        RequestKind::GetAltSetting { interface } => {
            let packet = match device::get_interface(io, interface).await {
                Ok(alt) => Packet::alt_setting_status(id, Status::Success, interface, alt),
                Err(error) => {
                    tracing::warn!(
                        id,
                        interface,
                        %error,
                        "RDPEUSB GET_INTERFACE failed"
                    );
                    Packet::alt_setting_status(id, Status::IoError, interface, 0)
                }
            };
            Ok(vec![packet])
        }
        RequestKind::SetAltSetting { interface, alt } => {
            match device::get_interface(io, interface).await {
                Ok(active) if active == alt => {
                    let mut packets = bridge.device_metadata_packets()?;
                    packets.push(Packet::alt_setting_status(
                        id,
                        Status::Success,
                        interface,
                        active,
                    ));
                    Ok(packets)
                }
                Ok(active) => {
                    tracing::warn!(
                        id,
                        interface,
                        requested = alt,
                        active,
                        "Rejected usbredir alternate-setting change without RDPEUSB handle rebinding"
                    );
                    Ok(vec![Packet::alt_setting_status(
                        id,
                        Status::Inval,
                        interface,
                        active,
                    )])
                }
                Err(error) => {
                    tracing::warn!(
                        id,
                        interface,
                        %error,
                        "RDPEUSB GET_INTERFACE failed"
                    );
                    Ok(vec![Packet::alt_setting_status(
                        id,
                        Status::IoError,
                        interface,
                        alt,
                    )])
                }
            }
        }
        RequestKind::StartIsoStream { endpoint, .. } | RequestKind::StopIsoStream { endpoint } => {
            Ok(vec![Packet::iso_stream_status(id, Status::Inval, endpoint)])
        }
        RequestKind::StartInterruptReceiving { endpoint }
        | RequestKind::StopInterruptReceiving { endpoint } => {
            Ok(vec![Packet::interrupt_receiving_status(
                id,
                Status::Inval,
                endpoint,
            )])
        }
        RequestKind::AllocBulkStreams {
            endpoints,
            no_streams,
        } => Ok(vec![Packet::bulk_streams_status(
            id,
            endpoints,
            no_streams,
            Status::Inval,
        )]),
        RequestKind::FreeBulkStreams { endpoints } => Ok(vec![Packet::bulk_streams_status(
            id,
            endpoints,
            0,
            Status::Inval,
        )]),
        RequestKind::StartBulkReceiving {
            stream_id,
            endpoint,
            ..
        }
        | RequestKind::StopBulkReceiving {
            stream_id,
            endpoint,
        } => Ok(vec![Packet::bulk_receiving_status(
            id,
            stream_id,
            endpoint,
            Status::Inval,
        )]),
        RequestKind::CancelDataPacket => {
            tracing::debug!(
                id,
                "Ignored usbredir cancellation because no asynchronous transfer is registered"
            );
            Ok(Vec::new())
        }
        kind => {
            tracing::warn!(id, ?kind, "Ignored unexpected usbredir request from QEMU");
            Ok(Vec::new())
        }
    }
}
