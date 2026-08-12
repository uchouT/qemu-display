//! usbredir data-transfer translation.

use anyhow::Context as _;
use bytes::Bytes;
use ironrdp_server::{UsbDeviceHandle, UsbRequestCompletion};
use ironrdp_usb::{
    control::{
        standard_request, Recipient, RequestKind as ControlRequestKind, RequestType, SetupPacket,
    },
    descriptor::{descriptor_type, ConfigurationDescriptorSet},
    endpoint::EndpointAddress,
    transfer::{BulkTransferRequest, ControlTransferRequest, InterruptTransferRequest},
    Direction,
};
use usbredir_proto::{DataKind, DataPacket, Endpoint, Packet, Status};

use super::bridge::usbredir_status;
use super::data_inflight::{DataInFlight, Purpose};
use super::device::ActiveDevice;

/// Everything answering one guest data packet needs and its RDPEUSB completion
/// does not carry.
pub(super) struct ResponseContext {
    pub(super) usbredir_id: u64,
    pub(super) reply: Reply,
}

/// How a response is addressed, as opposed to what it reports.
#[derive(Debug, Clone, Copy)]
pub(super) enum Reply {
    /// usbredir echoes the SETUP fields back in a control response.
    Control {
        endpoint: Endpoint,
        request: u8,
        request_type: u8,
        value: u16,
        index: u16,
        /// Requested length, used to clamp an over-long device response.
        length: u16,
    },
    Bulk {
        endpoint: Endpoint,
        stream_id: u32,
    },
    /// Only an interrupt OUT transfer is answered this way: usbredir delivers
    /// interrupt IN data through the receiving stream instead, which carries no
    /// guest packet ID.
    Interrupt {
        endpoint: Endpoint,
    },
}

/// `ENDPOINT_HALT`, the only standard endpoint feature selector.
const ENDPOINT_HALT: u16 = 0;

/// A standard descriptor qemu-rdp keeps a copy of.
enum CacheableDescriptor {
    Device,
    Configuration(u8),
}

/// One usbredir control packet, before translation.
struct ControlRequest {
    endpoint: Endpoint,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    length: u16,
    data: Bytes,
}

pub(super) async fn handle_data(
    usb: &UsbDeviceHandle,
    device: &ActiveDevice,
    inflight: &mut DataInFlight,
    packet: DataPacket,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let DataPacket {
        id,
        endpoint,
        status: _,
        kind,
        data,
    } = packet;

    match kind {
        DataKind::Control {
            request,
            requesttype,
            value,
            index,
            length,
        } => {
            let request = ControlRequest {
                endpoint,
                request,
                request_type: requesttype,
                value,
                index,
                length,
                data,
            };
            handle_control(usb, device, inflight, id, request, out).await
        }
        DataKind::Bulk { length, stream_id } => {
            handle_bulk(usb, inflight, id, endpoint, stream_id, length, data, out).await
        }
        DataKind::Interrupt { length } => {
            handle_interrupt(usb, inflight, id, endpoint, length, data, out).await
        }

        // usbredir reports isochronous failures through `iso_stream_status`
        // rather than per-packet responses, so an unanswered ID owes the guest
        // nothing.
        kind => {
            tracing::warn!(
                id,
                ?endpoint,
                ?kind,
                "Ignored unsupported usbredir data transfer"
            );
            Ok(())
        }
    }
}

/// Turns a finished data request into the response its packet ID owes QEMU.
pub(super) fn complete(
    device: &mut ActiveDevice,
    ctx: ResponseContext,
    result: anyhow::Result<UsbRequestCompletion>,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let ResponseContext {
        usbredir_id: id,
        reply,
    } = ctx;

    let completion = match result {
        Ok(UsbRequestCompletion::Transfer(completion)) => completion,
        Ok(_) => anyhow::bail!("usb data transfer produced an unexpected completion"),
        Err(error) => {
            // The request ended without a completion, so the device channel is
            // closing or the completion could not be decoded. The packet ID
            // still owes QEMU a response.
            tracing::warn!(id, %error, "USB data transfer ended without a completion");
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }
    };

    if let Err(error) = completion.status {
        tracing::warn!(id, %error, "USB data transfer failed");
        out.push(reply.failure(id, usbredir_status(error)));
        return Ok(());
    }

    if !reply.is_input() {
        // An OUT transfer carries no payload back; only the transferred length
        // is meaningful.
        if reply.exceeds_requested_length(completion.actual_length) {
            tracing::error!(
                id,
                actual_length = completion.actual_length,
                "USB OUT transfer reports more bytes than were submitted"
            );
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }

        out.push(reply.packet(id, Status::Success, completion.actual_length, Bytes::new()));
        return Ok(());
    }

    let mut data = completion.data;
    if reply.exceeds_requested_length(completion.actual_length) {
        tracing::warn!(
            id,
            actual_length = completion.actual_length,
            "Truncated an over-long USB IN transfer response"
        );
        data.truncate(reply.requested_length().unwrap_or(data.len()));
    }

    // A descriptor the guest read is worth keeping: a later SetConfiguration
    // then needs no extra round trip. A malformed one must not rewrite the
    // successful result the guest already earned.
    if let Err(error) = reply.cache_descriptor_response(device, &data) {
        tracing::warn!(id, %error, "Did not cache a successful USB descriptor response");
    }

    let length = u32::try_from(data.len()).context("usb transfer response is too large")?;
    out.push(reply.packet(id, Status::Success, length, Bytes::from(data)));
    Ok(())
}

async fn handle_control(
    usb: &UsbDeviceHandle,
    device: &ActiveDevice,
    inflight: &mut DataInFlight,
    id: u64,
    request: ControlRequest,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let reply = request.reply();
    tracing::trace!(
        id,
        ?reply,
        data_len = request.data.len(),
        "Handling usbredir control request"
    );

    if let Err(status) = request.validate() {
        tracing::warn!(
            id,
            ?reply,
            ?status,
            "Rejected malformed usbredir control request"
        );
        out.push(reply.failure(id, status));
        return Ok(());
    }

    if let Some(endpoint) = request.clear_halt_target() {
        return clear_halt(usb, id, reply, endpoint, out).await;
    }

    if let Some(data) = request.cached_descriptor_response(device) {
        tracing::trace!(
            id,
            actual_length = data.len(),
            "Answered usbredir control request from the descriptor cache"
        );
        let length = u32::try_from(data.len()).context("cached usb descriptor is too large")?;
        out.push(reply.packet(id, Status::Success, length, Bytes::from(data)));
        return Ok(());
    }

    let pending = usb
        .control_transfer(ControlTransferRequest {
            setup: request.setup(),
            data: request.data.to_vec(),
        })
        .await;
    let pending = match pending {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(id, ?reply, %error, "Failed to submit USB control transfer");
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }
    };

    inflight.push(
        pending,
        Purpose::Data(ResponseContext {
            usbredir_id: id,
            reply,
        }),
    )
}

/// Clears a stalled endpoint through the dedicated RDPEUSB pipe request.
///
/// Awaited inline: a stall is an error-recovery step and the guest retries the
/// endpoint immediately afterwards, so there is nothing to overlap it with.
async fn clear_halt(
    usb: &UsbDeviceHandle,
    id: u64,
    reply: Reply,
    endpoint: EndpointAddress,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let pending = match usb.clear_halt(endpoint).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(id, endpoint = endpoint.raw(), %error, "Failed to submit USB clear halt");
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }
    };

    let UsbRequestCompletion::Unit(result) = pending.wait().await? else {
        anyhow::bail!("usb clear halt produced an unexpected completion")
    };
    let status = match result {
        Ok(()) => Status::Success,
        Err(error) => {
            tracing::warn!(id, endpoint = endpoint.raw(), %error, "USB clear halt failed");
            usbredir_status(error)
        }
    };

    // CLEAR_FEATURE has no data stage, so the response carries no payload.
    out.push(reply.packet(id, status, 0, Bytes::new()));
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "one usbredir bulk packet's fields"
)]
async fn handle_bulk(
    usb: &UsbDeviceHandle,
    inflight: &mut DataInFlight,
    id: u64,
    endpoint: Endpoint,
    stream_id: u32,
    length: u32,
    data: Bytes,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let reply = Reply::Bulk {
        endpoint,
        stream_id,
    };

    // qemu-rdp does not advertise usb_redir_cap_bulk_streams. Silently treating
    // a non-zero stream as stream zero would send data to the wrong logical
    // stream if a peer violated capability negotiation.
    if stream_id != 0 {
        tracing::warn!(
            id,
            ?endpoint,
            stream_id,
            "Rejected unsupported USB bulk stream"
        );
        out.push(reply.failure(id, Status::Inval));
        return Ok(());
    }

    let address = match endpoint_address(id, endpoint) {
        Ok(address) => address,
        Err(status) => {
            out.push(reply.failure(id, status));
            return Ok(());
        }
    };

    // A submission failure is reported to the guest rather than ending the
    // session: an endpoint that is not part of the active configuration is a
    // guest mistake, not a transport fault.
    let pending = usb
        .bulk_transfer(BulkTransferRequest {
            endpoint: address,
            length,
            data: data.to_vec(),
        })
        .await;
    let pending = match pending {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(id, ?endpoint, %error, "Failed to submit USB bulk transfer");
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }
    };

    inflight.push(
        pending,
        Purpose::Data(ResponseContext {
            usbredir_id: id,
            reply,
        }),
    )
}

/// Submits an interrupt OUT transfer.
///
/// usbredir splits the two directions of an interrupt endpoint: an OUT
/// transfer is a normal request the guest submits and this bridge answers by
/// packet ID, while IN data is pushed by the host as an unsolicited stream
/// started by `StartInterruptReceiving`.
async fn handle_interrupt(
    usb: &UsbDeviceHandle,
    inflight: &mut DataInFlight,
    id: u64,
    endpoint: Endpoint,
    length: u16,
    data: Bytes,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let reply = Reply::Interrupt { endpoint };

    let address = match endpoint_address(id, endpoint) {
        Ok(address) => address,
        Err(status) => {
            out.push(reply.failure(id, status));
            return Ok(());
        }
    };

    let pending = usb
        .interrupt_transfer(InterruptTransferRequest {
            endpoint: address,
            length: u32::from(length),
            data: data.to_vec(),
        })
        .await;
    let pending = match pending {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(id, ?endpoint, %error, "Failed to submit USB interrupt transfer");
            out.push(reply.failure(id, Status::IoError));
            return Ok(());
        }
    };

    inflight.push(
        pending,
        Purpose::Data(ResponseContext {
            usbredir_id: id,
            reply,
        }),
    )
}

/// Converts a usbredir endpoint into the USB endpoint address the facade takes.
fn endpoint_address(id: u64, endpoint: Endpoint) -> Result<EndpointAddress, Status> {
    EndpointAddress::from_raw(endpoint.raw()).map_err(|error| {
        tracing::warn!(id, ?endpoint, %error, "Rejected invalid USB endpoint address");
        Status::Inval
    })
}

impl Reply {
    fn is_input(self) -> bool {
        match self {
            Self::Control { endpoint, .. }
            | Self::Bulk { endpoint, .. }
            | Self::Interrupt { endpoint } => endpoint.is_input(),
        }
    }

    /// The length the guest asked for, where usbredir bounds the response.
    fn requested_length(self) -> Option<usize> {
        match self {
            Self::Control { length, .. } => Some(usize::from(length)),
            Self::Bulk { .. } | Self::Interrupt { .. } => None,
        }
    }

    fn exceeds_requested_length(self, actual_length: u32) -> bool {
        let Some(requested) = self.requested_length() else {
            return false;
        };

        match usize::try_from(actual_length) {
            Ok(actual) => actual > requested,
            Err(_) => true,
        }
    }

    fn packet(self, id: u64, status: Status, length: u32, data: Bytes) -> Packet {
        match self {
            Self::Control {
                endpoint,
                request,
                request_type,
                value,
                index,
                ..
            } => Packet::control_packet(
                id,
                endpoint,
                request,
                request_type,
                status,
                value,
                index,
                u16::try_from(length).unwrap_or(u16::MAX),
                data,
            ),
            Self::Bulk {
                endpoint,
                stream_id,
            } => Packet::bulk_packet(id, endpoint, status, length, stream_id, data),
            Self::Interrupt { endpoint } => Packet::interrupt_packet(
                id,
                endpoint,
                status,
                u16::try_from(length).unwrap_or(u16::MAX),
                data,
            ),
        }
    }

    fn failure(self, id: u64, status: Status) -> Packet {
        self.packet(id, status, 0, Bytes::new())
    }

    /// The standard descriptor this request reads, if qemu-rdp caches it.
    fn cacheable_descriptor(self) -> Option<CacheableDescriptor> {
        let Self::Control {
            request,
            request_type,
            value,
            index,
            ..
        } = self
        else {
            return None;
        };

        let request_type = RequestType::from_raw(request_type);
        if request_type.direction() != Direction::In
            || request_type.kind() != ControlRequestKind::STANDARD
            || request_type.recipient() != Recipient::DEVICE
            || request != standard_request::GET_DESCRIPTOR
            || index != 0
        {
            return None;
        }

        let [descriptor_index, descriptor_type] = value.to_le_bytes();
        match (descriptor_type, descriptor_index) {
            (descriptor_type::DEVICE, 0) => Some(CacheableDescriptor::Device),
            (descriptor_type::CONFIGURATION, index) => {
                Some(CacheableDescriptor::Configuration(index))
            }
            _ => None,
        }
    }

    fn cache_descriptor_response(
        self,
        device: &mut ActiveDevice,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let Some(CacheableDescriptor::Configuration(index)) = self.cacheable_descriptor() else {
            return Ok(());
        };

        // A guest usually reads the nine-byte header first; only a complete
        // descriptor set is worth caching.
        if ConfigurationDescriptorSet::parse_if_complete(bytes)
            .context("parse usb configuration descriptor")?
            .is_none()
        {
            return Ok(());
        }

        device.insert_configuration_descriptor(index, bytes)
    }
}

impl ControlRequest {
    fn setup(&self) -> SetupPacket {
        SetupPacket {
            request_type: RequestType::from_raw(self.request_type),
            request: self.request,
            value: self.value,
            index: self.index,
            length: self.length,
        }
    }

    fn reply(&self) -> Reply {
        Reply::Control {
            endpoint: self.endpoint,
            request: self.request,
            request_type: self.request_type,
            value: self.value,
            index: self.index,
            length: self.length,
        }
    }

    fn validate(&self) -> Result<(), Status> {
        let setup = self.setup();
        let direction_in = setup.request_type.direction() == Direction::In;

        // usbredir addresses the default control pipe by direction, so the
        // endpoint has to agree with the SETUP packet it carries.
        if self.endpoint.number() != 0 || self.endpoint.is_input() != direction_in {
            return Err(Status::Inval);
        }

        // usbredir carries the stateful standard requests in its own packets,
        // and the device address belongs to the client's USB stack. Serving
        // them as raw control transfers would desync both observers of the
        // device state, so they are stalled the way a device would.
        if setup.request_type.kind() == ControlRequestKind::STANDARD
            && matches!(
                self.request,
                standard_request::SET_ADDRESS
                    | standard_request::SET_CONFIGURATION
                    | standard_request::SET_INTERFACE
            )
        {
            return Err(Status::Stall);
        }

        Ok(())
    }

    /// The endpoint a `CLEAR_FEATURE(ENDPOINT_HALT)` targets.
    ///
    /// Forwarding this as a plain control transfer would clear the halt on the
    /// device but leave the client's host-side pipe state untouched, so the
    /// endpoint would keep failing: Windows requires a stalled pipe to be
    /// cleared with `SYNC_RESET_PIPE_AND_CLEAR_STALL`. usbredirhost intercepts
    /// the same request, for the same reason, to call `libusb_clear_halt`.
    ///
    /// This matters in practice: bulk-only mass storage recovers from a stalled
    /// SCSI command exactly this way.
    fn clear_halt_target(&self) -> Option<EndpointAddress> {
        let request_type = RequestType::from_raw(self.request_type);
        if request_type.kind() != ControlRequestKind::STANDARD
            || request_type.recipient() != Recipient::ENDPOINT
            || self.request != standard_request::CLEAR_FEATURE
            || self.value != ENDPOINT_HALT
        {
            return None;
        }

        // For an endpoint recipient, `wIndex` carries the endpoint address.
        EndpointAddress::from_raw(self.index.to_le_bytes()[0]).ok()
    }

    /// Answers a descriptor read from the cache, avoiding a round trip.
    fn cached_descriptor_response(&self, device: &ActiveDevice) -> Option<Vec<u8>> {
        let bytes = match self.reply().cacheable_descriptor()? {
            CacheableDescriptor::Device => device.device_descriptor_bytes(),
            CacheableDescriptor::Configuration(index) => {
                device.configuration_descriptor_by_index(index)?
            }
        };

        let length = bytes.len().min(usize::from(self.length));
        Some(bytes[..length].to_vec())
    }
}
