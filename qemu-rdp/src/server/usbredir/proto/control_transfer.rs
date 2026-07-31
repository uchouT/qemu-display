//! USB control-transfer translation between usbredir and RDPEUSB.

use bytes::Bytes;
use ironrdp_rdpeusb::{
    io::{
        TransferInCompletionResult, TransferInPacket, TransferOutCompletionResult,
        TransferOutPacket, TsUrbInKind, TsUrbInPacket, TsUrbOutKind, TsUrbOutPacket, UrbFunction,
    },
    pdu::usb_dev::ts_urb::{
        TsUrbControlDescRequest, TsUrbControlFeatRequest, TsUrbControlGetConfigRequest,
        TsUrbControlGetInterfaceRequest, TsUrbControlGetStatusRequest,
        TsUrbControlVendorClassRequest,
    },
};
use usbredir_proto::{DataKind, DataPacket, Endpoint, Packet, Status};

use super::super::{rdpeusb_io::RdpeusbDeviceIo, RdpeusbCompletionKind};
use super::{
    descriptor::{self, ConfigurationDescriptor},
    state::ActiveDevice,
    transfer::{completion_status, PendingTransfers},
};

const USB_DIRECTION_IN: u8 = 0x80;
const USB_REQUEST_TYPE_MASK: u8 = 0x60;
const USB_REQUEST_TYPE_STANDARD: u8 = 0x00;
const USB_REQUEST_TYPE_CLASS: u8 = 0x20;
const USB_REQUEST_TYPE_VENDOR: u8 = 0x40;
const USB_RECIPIENT_MASK: u8 = 0x1f;
const USB_RECIPIENT_DEVICE: u8 = 0;
const USB_RECIPIENT_INTERFACE: u8 = 1;
const USB_RECIPIENT_ENDPOINT: u8 = 2;
const USB_RECIPIENT_OTHER: u8 = 3;
const USB_STANDARD_DEVICE_IN: u8 =
    USB_DIRECTION_IN | USB_REQUEST_TYPE_STANDARD | USB_RECIPIENT_DEVICE;

const USB_REQUEST_GET_STATUS: u8 = 0x00;
const USB_REQUEST_CLEAR_FEATURE: u8 = 0x01;
const USB_REQUEST_SET_FEATURE: u8 = 0x03;
const USB_REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQUEST_GET_CONFIGURATION: u8 = 0x08;
const USB_REQUEST_GET_INTERFACE: u8 = 0x0a;

// MS-RDPEUSB 2.2.9 uses bit zero for USBD_TRANSFER_DIRECTION_IN.
const USBD_TRANSFER_DIRECTION_IN: u32 = 0x0000_0001;

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

enum RdpeusbControlRequest {
    In(TransferInPacket),
    Out(TransferOutPacket),
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
        RdpeusbControlRequest::In(transfer) => (
            io.handle().transfer_in_request(transfer).await,
            PendingControl::In(request),
        ),
        RdpeusbControlRequest::Out(transfer) => (
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
        if self.request_type != USB_STANDARD_DEVICE_IN
            || self.request != USB_REQUEST_GET_DESCRIPTOR
            || self.index != 0
        {
            return None;
        }

        let [index, descriptor_type] = self.value.to_le_bytes();
        match (descriptor_type, index) {
            (descriptor::DEVICE_DESCRIPTOR_TYPE, 0) => Some(CacheableDescriptor::Device),
            (descriptor::CONFIG_DESCRIPTOR_TYPE, index) => {
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

fn rdpeusb_request(request: &ControlRequest) -> Result<RdpeusbControlRequest, Status> {
    // Prefer the handle-free, operation-specific URBs used by MS-RDPEUSB
    // 2.2.9.9 through 2.2.9.14. Requests without a typed translation are
    // rejected until the generic TS_URB_CONTROL_TRANSFER fallback is added.
    let direction_in = request.request_type & USB_DIRECTION_IN != 0;
    if request.endpoint.number() != 0 || request.endpoint.is_input() != direction_in {
        return Err(Status::Inval);
    }
    if direction_in {
        if !request.data.is_empty() {
            return Err(Status::Inval);
        }
    } else if request.data.len() != usize::from(request.length) {
        return Err(Status::Inval);
    }

    let request_type = request.request_type & USB_REQUEST_TYPE_MASK;
    let recipient = request.request_type & USB_RECIPIENT_MASK;
    let output_buffer_size = u32::from(request.length);

    let (kind, func) = match (direction_in, request_type, request.request, recipient) {
        (true, USB_REQUEST_TYPE_STANDARD, USB_REQUEST_GET_DESCRIPTOR, recipient) => {
            let func = match recipient {
                USB_RECIPIENT_DEVICE => UrbFunction::URB_FUNCTION_GET_DESCRIPTOR_FROM_DEVICE,
                USB_RECIPIENT_INTERFACE => UrbFunction::URB_FUNCTION_GET_DESCRIPTOR_FROM_INTERFACE,
                USB_RECIPIENT_ENDPOINT => UrbFunction::URB_FUNCTION_GET_DESCRIPTOR_FROM_ENDPOINT,
                _ => return Err(Status::Stall),
            };
            let [index, desc_type] = request.value.to_le_bytes();
            (
                TsUrbInKind::CtlDescReq(TsUrbControlDescRequest {
                    index,
                    desc_type,
                    lang_id: request.index,
                }),
                func,
            )
        }
        (true, USB_REQUEST_TYPE_STANDARD, USB_REQUEST_GET_STATUS, recipient)
            if request.length == 2 =>
        {
            let func = match recipient {
                USB_RECIPIENT_DEVICE => UrbFunction::URB_FUNCTION_GET_STATUS_FROM_DEVICE,
                USB_RECIPIENT_INTERFACE => UrbFunction::URB_FUNCTION_GET_STATUS_FROM_INTERFACE,
                USB_RECIPIENT_ENDPOINT => UrbFunction::URB_FUNCTION_GET_STATUS_FROM_ENDPOINT,
                USB_RECIPIENT_OTHER => UrbFunction::URB_FUNCTION_GET_STATUS_FROM_OTHER,
                _ => return Err(Status::Stall),
            };
            (
                TsUrbInKind::CtlGetStatus(TsUrbControlGetStatusRequest {
                    index: request.index,
                }),
                func,
            )
        }
        (
            false,
            USB_REQUEST_TYPE_STANDARD,
            USB_REQUEST_CLEAR_FEATURE | USB_REQUEST_SET_FEATURE,
            recipient,
        ) if request.length == 0 => {
            let func = feature_function(request.request, recipient).ok_or(Status::Stall)?;
            (
                TsUrbInKind::CtlFeatReq(TsUrbControlFeatRequest {
                    feat_selector: request.value,
                    index: request.index,
                }),
                func,
            )
        }
        (true, USB_REQUEST_TYPE_STANDARD, USB_REQUEST_GET_CONFIGURATION, USB_RECIPIENT_DEVICE)
            if request.length == 1 =>
        {
            (
                TsUrbInKind::CtlGetConfig(TsUrbControlGetConfigRequest),
                UrbFunction::URB_FUNCTION_GET_CONFIGURATION,
            )
        }
        (true, USB_REQUEST_TYPE_STANDARD, USB_REQUEST_GET_INTERFACE, USB_RECIPIENT_INTERFACE)
            if request.length == 1 =>
        {
            (
                TsUrbInKind::CtlGetIface(TsUrbControlGetInterfaceRequest {
                    interface: request.index,
                }),
                UrbFunction::URB_FUNCTION_GET_INTERFACE,
            )
        }
        (true, USB_REQUEST_TYPE_CLASS | USB_REQUEST_TYPE_VENDOR, _, recipient) => {
            let func = vendor_class_function(request_type, recipient).ok_or(Status::Stall)?;
            (
                TsUrbInKind::VendorClassReq(TsUrbControlVendorClassRequest {
                    transfer_flags: USBD_TRANSFER_DIRECTION_IN,
                    request: request.request,
                    value: request.value,
                    index: request.index,
                }),
                func,
            )
        }
        (false, USB_REQUEST_TYPE_CLASS | USB_REQUEST_TYPE_VENDOR, _, recipient) => {
            let func = vendor_class_function(request_type, recipient).ok_or(Status::Stall)?;
            return Ok(RdpeusbControlRequest::Out(TransferOutPacket {
                ts_urb: TsUrbOutPacket {
                    kind: TsUrbOutKind::VendorClassReq(TsUrbControlVendorClassRequest {
                        transfer_flags: 0,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                    }),
                    no_ack: false,
                    func,
                },
                output_buffer: request.data.to_vec(),
            }));
        }
        _ => return Err(Status::Stall),
    };

    Ok(RdpeusbControlRequest::In(TransferInPacket {
        ts_urb: TsUrbInPacket { kind, func },
        output_buffer_size,
    }))
}

fn feature_function(request: u8, recipient: u8) -> Option<UrbFunction> {
    match (request, recipient) {
        (USB_REQUEST_CLEAR_FEATURE, USB_RECIPIENT_DEVICE) => {
            Some(UrbFunction::URB_FUNCTION_CLEAR_FEATURE_TO_DEVICE)
        }
        (USB_REQUEST_CLEAR_FEATURE, USB_RECIPIENT_INTERFACE) => {
            Some(UrbFunction::URB_FUNCTION_CLEAR_FEATURE_TO_INTERFACE)
        }
        (USB_REQUEST_CLEAR_FEATURE, USB_RECIPIENT_ENDPOINT) => {
            Some(UrbFunction::URB_FUNCTION_CLEAR_FEATURE_TO_ENDPOINT)
        }
        (USB_REQUEST_CLEAR_FEATURE, USB_RECIPIENT_OTHER) => {
            Some(UrbFunction::URB_FUNCTION_CLEAR_FEATURE_TO_OTHER)
        }
        (USB_REQUEST_SET_FEATURE, USB_RECIPIENT_DEVICE) => {
            Some(UrbFunction::URB_FUNCTION_SET_FEATURE_TO_DEVICE)
        }
        (USB_REQUEST_SET_FEATURE, USB_RECIPIENT_INTERFACE) => {
            Some(UrbFunction::URB_FUNCTION_SET_FEATURE_TO_INTERFACE)
        }
        (USB_REQUEST_SET_FEATURE, USB_RECIPIENT_ENDPOINT) => {
            Some(UrbFunction::URB_FUNCTION_SET_FEATURE_TO_ENDPOINT)
        }
        (USB_REQUEST_SET_FEATURE, USB_RECIPIENT_OTHER) => {
            Some(UrbFunction::URB_FUNCTION_SET_FEATURE_TO_OTHER)
        }
        _ => None,
    }
}

fn vendor_class_function(request_type: u8, recipient: u8) -> Option<UrbFunction> {
    match (request_type, recipient) {
        (USB_REQUEST_TYPE_VENDOR, USB_RECIPIENT_DEVICE) => {
            Some(UrbFunction::URB_FUNCTION_VENDOR_DEVICE)
        }
        (USB_REQUEST_TYPE_VENDOR, USB_RECIPIENT_INTERFACE) => {
            Some(UrbFunction::URB_FUNCTION_VENDOR_INTERFACE)
        }
        (USB_REQUEST_TYPE_VENDOR, USB_RECIPIENT_ENDPOINT) => {
            Some(UrbFunction::URB_FUNCTION_VENDOR_ENDPOINT)
        }
        (USB_REQUEST_TYPE_VENDOR, USB_RECIPIENT_OTHER) => {
            Some(UrbFunction::URB_FUNCTION_VENDOR_OTHER)
        }
        (USB_REQUEST_TYPE_CLASS, USB_RECIPIENT_DEVICE) => {
            Some(UrbFunction::URB_FUNCTION_CLASS_DEVICE)
        }
        (USB_REQUEST_TYPE_CLASS, USB_RECIPIENT_INTERFACE) => {
            Some(UrbFunction::URB_FUNCTION_CLASS_INTERFACE)
        }
        (USB_REQUEST_TYPE_CLASS, USB_RECIPIENT_ENDPOINT) => {
            Some(UrbFunction::URB_FUNCTION_CLASS_ENDPOINT)
        }
        (USB_REQUEST_TYPE_CLASS, USB_RECIPIENT_OTHER) => {
            Some(UrbFunction::URB_FUNCTION_CLASS_OTHER)
        }
        _ => None,
    }
}
