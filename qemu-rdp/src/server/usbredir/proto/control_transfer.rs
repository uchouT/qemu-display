//! USB control-transfer translation between usbredir and RDPEUSB.

use bytes::Bytes;
use ironrdp_rdpeusb::{
    io::{TransferInCompletionResult, TransferOutCompletionResult},
    usb as rdpeusb,
};
use ironrdp_usb::{
    control::{standard_request, Recipient, RequestKind, RequestType, SetupPacket},
    descriptor::descriptor_type,
    value::Direction,
};
use usbredir_proto::{DataKind, DataPacket, Endpoint, Packet, Status};

use super::super::{rdpeusb_io::RdpeusbDeviceIo, RdpeusbCompletionKind};
use super::{
    descriptor::ConfigurationDescriptor,
    state::ActiveDevice,
    transfer::{completion_status, PendingTransfers},
};

#[derive(Debug)]
pub(super) struct ControlRequest {
    id: u64,
    endpoint: Endpoint,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    length: u16,
    data: Bytes,
}

#[derive(Debug)]
pub(super) enum PendingControl {
    In(ControlRequest),
    Out(ControlRequest),
}

enum CacheableDescriptor {
    Device,
    Configuration(u8),
}

pub(super) async fn handle(
    active_device: &ActiveDevice,
    packet: DataPacket,
    io: &RdpeusbDeviceIo,
    pending: &mut PendingTransfers,
) -> anyhow::Result<Vec<Packet>> {
    let DataPacket {
        id,
        endpoint,
        status: _,
        kind,
        data,
    } = packet;

    let DataKind::Control {
        request,
        requesttype,
        value,
        index,
        length,
    } = kind
    else {
        unreachable!("control-transfer handler received another data-packet kind");
    };

    let request = ControlRequest {
        id,
        endpoint,
        request,
        request_type: requesttype,
        value,
        index,
        length,
        data,
    };
    tracing::trace!(
        id,
        ?endpoint,
        request = request.request,
        request_type = request.request_type,
        value = request.value,
        index = request.index,
        length = request.length,
        "Handling usbredir control request"
    );

    let transfer = match rdpeusb_request(&request) {
        Ok(transfer) => transfer,
        Err(status) => {
            tracing::warn!(
                id,
                request = request.request,
                request_type = request.request_type,
                ?status,
                "Rejected unsupported or malformed usbredir control request"
            );
            return Ok(vec![request.response(status, Vec::new())]);
        }
    };

    if let Some(data) = request.cached_descriptor_response(active_device) {
        tracing::trace!(
            id,
            actual_length = data.len(),
            "Completed usbredir control request from the descriptor cache"
        );
        return Ok(vec![request.response(Status::Success, data)]);
    }

    // This await is only the RDPEUSB transport submission barrier. The device
    // result is dispatched later by the outer SessionTask event pump.
    let (sent, pending_control) = match transfer {
        rdpeusb::TransferRequest::In(transfer) => (
            io.handle().transfer_in_request(transfer).await,
            PendingControl::In(request),
        ),
        rdpeusb::TransferRequest::Out(transfer) => (
            io.handle().transfer_out_request(transfer).await,
            PendingControl::Out(request),
        ),
    };
    let sent = match sent {
        Ok(sent) => sent,
        Err(error) => {
            let request = pending_control.request();
            tracing::warn!(
                id = request.id,
                request = request.request,
                request_type = request.request_type,
                %error,
                "Failed to submit RDPEUSB control request"
            );
            return Ok(vec![pending_control.response(Status::IoError, Vec::new())]);
        }
    };
    if !sent.expects_completion {
        let request = pending_control.request();
        tracing::error!(
            id = request.id,
            request_id = sent.request_id,
            "RDPEUSB control transfer unexpectedly has no completion"
        );
        return Ok(vec![pending_control.response(Status::IoError, Vec::new())]);
    }
    pending.insert_control(sent.request_id, pending_control)?;

    Ok(Vec::new())
}

pub(super) fn complete(
    active_device: &mut ActiveDevice,
    pending: PendingControl,
    completion: RdpeusbCompletionKind,
) -> Packet {
    match (pending, completion) {
        (PendingControl::In(request), RdpeusbCompletionKind::TransferIn(completion)) => {
            complete_in(active_device, request, completion)
        }
        (PendingControl::Out(request), RdpeusbCompletionKind::TransferOut(completion)) => {
            complete_out(request, completion)
        }
        (pending, completion) => {
            tracing::error!(
                id = pending.request().id,
                completion_kind = completion.name(),
                "RDPEUSB completion kind does not match usbredir control request"
            );
            pending.response(Status::IoError, Vec::new())
        }
    }
}

fn complete_in(
    active_device: &mut ActiveDevice,
    request: ControlRequest,
    completion: TransferInCompletionResult,
) -> Packet {
    let status = completion_status(
        completion.hresult,
        completion.ts_urb_result.header.usbd_status,
    );
    if status != Status::Success {
        tracing::warn!(
            id = request.id,
            request = request.request,
            request_type = request.request_type,
            hresult = completion.hresult,
            usbd_status = completion.ts_urb_result.header.usbd_status,
            ?status,
            "RDPEUSB control request failed"
        );
        return request.response(status, Vec::new());
    }

    let mut data = completion.output_buffer;
    data.truncate(usize::from(request.length));
    if let Err(error) = request.cache_configuration_response(active_device, &data) {
        // The transfer itself succeeded. A malformed descriptor must not
        // rewrite that guest-visible result.
        tracing::warn!(
            id = request.id,
            request = request.request,
            value = request.value,
            index = request.index,
            %error,
            "Did not cache a successful USB descriptor response"
        );
    }
    tracing::trace!(
        id = request.id,
        actual_length = data.len(),
        "Completed usbredir control request"
    );
    request.response(Status::Success, data)
}

fn complete_out(request: ControlRequest, completion: TransferOutCompletionResult) -> Packet {
    let status = completion_status(
        completion.hresult,
        completion.ts_urb_result.header.usbd_status,
    );
    if status != Status::Success {
        tracing::warn!(
            id = request.id,
            request = request.request,
            request_type = request.request_type,
            hresult = completion.hresult,
            usbd_status = completion.ts_urb_result.header.usbd_status,
            ?status,
            "RDPEUSB control request failed"
        );
        return request.response(status, Vec::new());
    }

    if completion.output_buffer_size > u32::from(request.length) {
        tracing::error!(
            id = request.id,
            requested_length = request.length,
            actual_length = completion.output_buffer_size,
            "RDPEUSB control OUT completion exceeds the submitted buffer"
        );
        return request.response(Status::IoError, Vec::new());
    }

    let actual_length = completion.output_buffer_size as u16;
    tracing::trace!(
        id = request.id,
        actual_length,
        "Completed usbredir control OUT request"
    );
    request.response_with_length(Status::Success, actual_length, Vec::new())
}

impl PendingControl {
    pub(super) fn usbredir_id(&self) -> u64 {
        self.request().id
    }

    fn request(&self) -> &ControlRequest {
        match self {
            Self::In(request) | Self::Out(request) => request,
        }
    }

    fn response(&self, status: Status, data: Vec<u8>) -> Packet {
        self.request().response(status, data)
    }
}

impl ControlRequest {
    fn response(&self, status: Status, data: Vec<u8>) -> Packet {
        let length = u16::try_from(data.len()).unwrap_or(u16::MAX);
        self.response_with_length(status, length, data)
    }

    fn response_with_length(&self, status: Status, length: u16, data: Vec<u8>) -> Packet {
        Packet::control_packet(
            self.id,
            self.endpoint,
            self.request,
            self.request_type,
            status,
            self.value,
            self.index,
            length,
            data,
        )
    }

    fn cacheable_descriptor(&self) -> Option<CacheableDescriptor> {
        let request_type = RequestType::from_raw(self.request_type);
        if request_type.direction() != Direction::In
            || request_type.kind() != RequestKind::STANDARD
            || request_type.recipient() != Recipient::DEVICE
            || self.request != standard_request::GET_DESCRIPTOR
            || self.index != 0
        {
            return None;
        }

        let [index, descriptor_type] = self.value.to_le_bytes();
        match (descriptor_type, index) {
            (descriptor_type::DEVICE, 0) => Some(CacheableDescriptor::Device),
            (descriptor_type::CONFIGURATION, index) => {
                Some(CacheableDescriptor::Configuration(index))
            }
            _ => None,
        }
    }

    fn cached_descriptor_response(&self, active_device: &ActiveDevice) -> Option<Vec<u8>> {
        let bytes = match self.cacheable_descriptor()? {
            CacheableDescriptor::Device => active_device.device_descriptor_bytes(),
            CacheableDescriptor::Configuration(index) => active_device
                .configuration_descriptor_by_index(index)?
                .bytes(),
        };
        let response_length = bytes.len().min(usize::from(self.length));
        Some(bytes[..response_length].to_vec())
    }

    fn cache_configuration_response(
        &self,
        active_device: &mut ActiveDevice,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let Some(CacheableDescriptor::Configuration(index)) = self.cacheable_descriptor() else {
            return Ok(());
        };
        let Some(descriptor) = ConfigurationDescriptor::parse_if_complete(bytes)? else {
            return Ok(());
        };
        active_device.insert_configuration_descriptor(index, descriptor)
    }
}

fn rdpeusb_request(request: &ControlRequest) -> Result<rdpeusb::TransferRequest, Status> {
    let setup = SetupPacket {
        request_type: RequestType::from_raw(request.request_type),
        request: request.request,
        value: request.value,
        index: request.index,
        length: request.length,
    };
    let direction_in = setup.request_type.direction() == Direction::In;
    if request.endpoint.number() != 0 || request.endpoint.is_input() != direction_in {
        return Err(Status::Inval);
    }

    rdpeusb::control_transfer(setup, request.data.to_vec()).map_err(|error| match error {
        rdpeusb::ConversionError::InTransferHasData { .. }
        | rdpeusb::ConversionError::OutTransferLengthMismatch { .. } => Status::Inval,
        rdpeusb::ConversionError::StatefulStandardRequest { .. } => Status::Stall,
        _ => Status::IoError,
    })
}
