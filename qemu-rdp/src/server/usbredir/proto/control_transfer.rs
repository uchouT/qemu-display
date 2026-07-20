//! USB control-transfer translation between usbredir and RDPEUSB.

use anyhow::ensure;
use bytes::Bytes;
use ironrdp_rdpeusb::{
    io::{TransferInCompletionResult, TransferInPacket, TsUrbInKind, TsUrbInPacket, UrbFunction},
    pdu::usb_dev::ts_urb::{
        TsUrbControlDescRequest, TsUrbControlFeatRequest, TsUrbControlGetConfigRequest,
        TsUrbControlGetInterfaceRequest, TsUrbControlGetStatusRequest,
        TsUrbControlVendorClassRequest,
    },
};
use usbredir_proto::{DataKind, DataPacket, Endpoint, Packet, Status};

use super::super::rdpeusb_io::RdpeusbDeviceIo;

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

const USB_REQUEST_GET_STATUS: u8 = 0x00;
const USB_REQUEST_CLEAR_FEATURE: u8 = 0x01;
const USB_REQUEST_SET_FEATURE: u8 = 0x03;
const USB_REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQUEST_GET_CONFIGURATION: u8 = 0x08;
const USB_REQUEST_GET_INTERFACE: u8 = 0x0a;

// MS-RDPEUSB 2.2.9 uses bit zero for USBD_TRANSFER_DIRECTION_IN.
const USBD_TRANSFER_DIRECTION_IN: u32 = 0x0000_0001;

struct ControlRequest {
    id: u64,
    endpoint: Endpoint,
    request: u8,
    request_type: u8,
    value: u16,
    index: u16,
    length: u16,
    data: Bytes,
}

pub(super) async fn handle(
    packet: DataPacket,
    io: &mut RdpeusbDeviceIo,
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

    let packet = match io
        .wait_transfer_in(transfer)
        .await
        .and_then(successful_transfer_in_output)
    {
        Ok(mut data) => {
            data.truncate(usize::from(request.length));
            tracing::trace!(
                id,
                actual_length = data.len(),
                "Completed usbredir control request"
            );
            request.response(Status::Success, data)
        }
        Err(error) => {
            tracing::warn!(
                id,
                request = request.request,
                request_type = request.request_type,
                %error,
                "RDPEUSB control request failed"
            );
            request.response(Status::IoError, Vec::new())
        }
    };

    Ok(vec![packet])
}

impl ControlRequest {
    fn response(&self, status: Status, data: Vec<u8>) -> Packet {
        let length = u16::try_from(data.len()).unwrap_or(u16::MAX);
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
}

fn rdpeusb_request(request: &ControlRequest) -> Result<TransferInPacket, Status> {
    // Prefer the handle-free, operation-specific URBs used by MS-RDPEUSB
    // 2.2.9.9 through 2.2.9.14. A generic TS_URB_CONTROL_TRANSFER requires a
    // pipe handle obtained by SELECT_CONFIGURATION/SELECT_INTERFACE, which is
    // deliberately outside this read-only enumeration stage.
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
        _ => return Err(Status::Stall),
    };

    Ok(TransferInPacket {
        ts_urb: TsUrbInPacket { kind, func },
        output_buffer_size,
    })
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

fn successful_transfer_in_output(
    completion: TransferInCompletionResult,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        completion.hresult == 0,
        "RDPEUSB transfer-in failed with HRESULT {:#010x}",
        completion.hresult
    );
    ensure!(
        completion.ts_urb_result.header.usbd_status == 0,
        "USB transfer-in failed with USBD status {:#010x}",
        completion.ts_urb_result.header.usbd_status
    );
    Ok(completion.output_buffer)
}
