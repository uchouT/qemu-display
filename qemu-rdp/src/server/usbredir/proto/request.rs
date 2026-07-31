//! Translation of usbredir request packets into synchronous RDPEUSB operations.

use usbredir_proto::{Packet, RequestKind, RequestPacket, Status};

use super::super::rdpeusb_io::RdpeusbDeviceIo;
use super::{
    device, device_metadata_packets,
    state::ActiveDevice,
    transfer::{self, PendingTransfers},
};

pub(super) async fn handle_control(
    active_device: &mut ActiveDevice,
    request: RequestPacket,
    io: &mut RdpeusbDeviceIo,
    pending: &mut PendingTransfers,
) -> anyhow::Result<Vec<Packet>> {
    let RequestPacket { id, kind } = request;

    match kind {
        RequestKind::Reset => {
            // TODO: Send the RDPEUSB port-reset IOCTL, quiesce every pending
            // transfer/stream, invalidate all handles, and rebuild the
            // observed view. usbredir has no Reset response.
            tracing::debug!(id, "RDPEUSB port reset is not implemented");
            Ok(Vec::new())
        }
        RequestKind::GetConfiguration => Ok(get_configuration(active_device, id)),
        RequestKind::SetConfiguration { configuration } => {
            set_configuration(active_device, id, configuration, io).await
        }
        RequestKind::GetAltSetting { interface } => {
            Ok(get_alt_setting(active_device, id, interface))
        }
        RequestKind::SetAltSetting { interface, alt } => {
            set_alt_setting(active_device, id, interface, alt, io).await
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
            transfer::cancel_data_packet(id, io, pending)?;
            Ok(Vec::new())
        }
        kind => {
            tracing::warn!(id, ?kind, "Ignored unexpected usbredir request from QEMU");
            Ok(Vec::new())
        }
    }
}

fn get_configuration(active_device: &ActiveDevice, id: u64) -> Vec<Packet> {
    // QEMU queries the guest-visible state that this bridge owns. Importing
    // the client's physical state here would make a read change the authority
    // used by the following SetConfiguration transaction.
    vec![Packet::configuration_status(
        id,
        Status::Success,
        active_device.configuration(),
    )]
}

async fn set_configuration(
    active_device: &mut ActiveDevice,
    id: u64,
    configuration: u8,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<Vec<Packet>> {
    let fallback = active_device.configuration();
    if !active_device.has_configuration(configuration) {
        let uncached_indices = (0..active_device.num_configurations())
            .filter(|&index| {
                active_device
                    .configuration_descriptor_by_index(index)
                    .is_none()
            })
            .collect::<Vec<_>>();
        match device::get_config_descriptor(io, &uncached_indices, configuration).await {
            Ok(Some((index, descriptor))) => {
                if let Err(error) = active_device.insert_configuration_descriptor(index, descriptor)
                {
                    tracing::warn!(
                        id,
                        configuration,
                        descriptor_index = index,
                        %error,
                        "Could not cache USB configuration descriptor"
                    );
                    return Ok(vec![Packet::configuration_status(
                        id,
                        Status::IoError,
                        fallback,
                    )]);
                }
            }
            Ok(None) => {
                tracing::warn!(id, configuration, "USB configuration does not exist");
                return Ok(vec![Packet::configuration_status(
                    id,
                    Status::Inval,
                    fallback,
                )]);
            }
            Err(error) => {
                tracing::warn!(
                    id,
                    configuration,
                    %error,
                    "Failed to load USB configuration descriptor"
                );
                return Ok(vec![Packet::configuration_status(
                    id,
                    Status::IoError,
                    fallback,
                )]);
            }
        }
    }

    let plan = match active_device.plan_configuration(configuration) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(id, configuration, %error, "Invalid USB configuration model");
            return Ok(vec![Packet::configuration_status(
                id,
                Status::IoError,
                fallback,
            )]);
        }
    };
    let request = match plan.select_configuration_request() {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(id, configuration, %error, "Failed to build SELECT_CONFIGURATION");
            return Ok(vec![Packet::configuration_status(
                id,
                Status::IoError,
                fallback,
            )]);
        }
    };

    // A same-value SetConfiguration is still a real selection transaction:
    // it resets endpoints and replaces the opaque RDPEUSB handles used by the
    // data plane.
    let token = active_device.begin_configuration_selection()?;
    let result = match device::select_configuration(io, request).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(id, configuration, %error, "RDPEUSB SELECT_CONFIGURATION failed");
            return Ok(vec![Packet::configuration_status(
                id,
                Status::IoError,
                fallback,
            )]);
        }
    };
    let binding = match active_device.validate_configuration_result(&plan, result, token) {
        Ok(binding) => binding,
        Err(error) => {
            tracing::warn!(
                id,
                configuration,
                %error,
                "Rejected inconsistent SELECT_CONFIGURATION result"
            );
            return Ok(vec![Packet::configuration_status(
                id,
                Status::IoError,
                fallback,
            )]);
        }
    };
    active_device.commit_configuration(plan, binding, token)?;

    // usbredir v0.7 requires metadata to precede the successful status.
    let mut packets = device_metadata_packets(active_device);
    packets.push(Packet::configuration_status(
        id,
        Status::Success,
        configuration,
    ));
    Ok(packets)
}

fn get_alt_setting(active_device: &ActiveDevice, id: u64, interface: u8) -> Vec<Packet> {
    let Some(alt) = active_device.alt_setting(interface) else {
        return vec![Packet::alt_setting_status(id, Status::Inval, interface, 0)];
    };
    vec![Packet::alt_setting_status(
        id,
        Status::Success,
        interface,
        alt,
    )]
}

async fn set_alt_setting(
    active_device: &mut ActiveDevice,
    id: u64,
    interface: u8,
    alt: u8,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<Vec<Packet>> {
    let Some(fallback) = active_device.alt_setting(interface) else {
        return Ok(vec![Packet::alt_setting_status(
            id,
            Status::Inval,
            interface,
            0,
        )]);
    };
    if !active_device.has_alt_setting(interface, alt) {
        return Ok(vec![Packet::alt_setting_status(
            id,
            Status::Inval,
            interface,
            fallback,
        )]);
    }

    let plan = match active_device.plan_alt_setting(interface, alt) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(id, interface, alt, %error, "Invalid alternate-setting model");
            return Ok(vec![Packet::alt_setting_status(
                id,
                Status::IoError,
                interface,
                fallback,
            )]);
        }
    };

    let config_handle = match active_device.configuration_handle_for_interface_selection() {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(
                id,
                interface,
                alt,
                %error,
                "Cannot select USB interface without a valid configuration binding"
            );
            return Ok(vec![Packet::alt_setting_status(
                id,
                Status::IoError,
                interface,
                fallback,
            )]);
        }
    };
    let request = match plan.select_interface_request(config_handle, interface) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(id, interface, alt, %error, "Failed to build SELECT_INTERFACE");
            return Ok(vec![Packet::alt_setting_status(
                id,
                Status::IoError,
                interface,
                fallback,
            )]);
        }
    };
    let token = active_device.begin_interface_selection(interface)?;
    let result = match device::select_interface(io, request).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(id, interface, alt, %error, "RDPEUSB SELECT_INTERFACE failed");
            return Ok(vec![Packet::alt_setting_status(
                id,
                Status::IoError,
                interface,
                fallback,
            )]);
        }
    };
    let binding = match active_device.validate_interface_result(&plan, interface, result, token) {
        Ok(binding) => binding,
        Err(error) => {
            tracing::warn!(
                id,
                interface,
                alt,
                %error,
                "Rejected inconsistent SELECT_INTERFACE result"
            );
            return Ok(vec![Packet::alt_setting_status(
                id,
                Status::IoError,
                interface,
                fallback,
            )]);
        }
    };
    active_device.commit_alt_setting(plan, interface, binding, token)?;

    let mut packets = device_metadata_packets(active_device);
    packets.push(Packet::alt_setting_status(
        id,
        Status::Success,
        interface,
        alt,
    ));
    Ok(packets)
}
