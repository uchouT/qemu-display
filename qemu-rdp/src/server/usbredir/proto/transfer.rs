//! usbredir data-transfer routing and RDPEUSB completion dispatch.

use std::collections::HashMap;

use bytes::Bytes;
use ironrdp_rdpeusb::{io::RequestId, pdu::utils::PipeHandle, usb as rdpeusb};
use usbredir_proto::{DataKind, DataPacket, Endpoint, Packet, Status};

use super::super::{
    rdpeusb_io::RdpeusbDeviceIo, RdpeusbCompletion, RdpeusbCompletionKind, RedirEvent,
};
use super::{control_transfer, state::ActiveDevice};

// WDK USBD_STATUS values returned in TS_URB_RESULT_HEADER.
const USBD_STATUS_CANCELED: u32 = 0xc001_0000;
const USBD_STATUS_STALL_PID: u32 = 0xc000_0004;
const USBD_STATUS_BABBLE_DETECTED: u32 = 0xc000_0012;
const USBD_STATUS_TIMEOUT: u32 = 0xc000_6000;

/// In-flight usbredir data packets indexed in both protocol ID namespaces.
///
/// RDPEUSB completions are dispatched by `RequestId`, while
/// `usb_redir_cancel_data_packet` refers to the original usbredir ID. Keeping
/// both indexes here gives every asynchronous data-transfer kind one owner and
/// one completion path.
#[derive(Default)]
pub(super) struct PendingTransfers {
    by_rdpeusb: HashMap<RequestId, PendingEntry>,
    by_usbredir: HashMap<u64, RequestId>,
}

struct PendingEntry {
    transfer: PendingTransfer,
    cancel_requested: bool,
}

enum PendingTransfer {
    Control(control_transfer::PendingControl),
    Bulk(PendingBulk),
}

/// usbredir fields that are not carried by an RDPEUSB completion.
///
/// IronRDP separately retains the RDPEUSB request metadata needed to validate
/// and decode the wire completion. This context only correlates the decoded
/// result with the QEMU request that must be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingBulk {
    usbredir_id: u64,
    endpoint: Endpoint,
    stream_id: u32,
}

pub(super) async fn handle_data(
    active_device: &mut ActiveDevice,
    packet: DataPacket,
    io: &mut RdpeusbDeviceIo,
    pending: &mut PendingTransfers,
) -> anyhow::Result<Vec<Packet>> {
    anyhow::ensure!(
        !pending.by_usbredir.contains_key(&packet.id),
        "usbredir data packet ID {} is already pending",
        packet.id
    );

    if matches!(&packet.kind, DataKind::Control { .. }) {
        return control_transfer::handle(active_device, packet, io, pending).await;
    }

    if matches!(&packet.kind, DataKind::Bulk { .. }) {
        return handle_bulk(active_device, packet, io, pending).await;
    }

    tracing::warn!(
        id = packet.id,
        endpoint = ?packet.endpoint,
        kind = ?packet.kind,
        "Ignored usbredir data transfer whose RDPEUSB translation is not implemented"
    );
    Ok(Vec::new())
}

async fn handle_bulk(
    active_device: &ActiveDevice,
    packet: DataPacket,
    io: &RdpeusbDeviceIo,
    pending_transfers: &mut PendingTransfers,
) -> anyhow::Result<Vec<Packet>> {
    let DataPacket {
        id,
        endpoint,
        status: _,
        kind,
        data,
    } = packet;
    let DataKind::Bulk { length, stream_id } = kind else {
        unreachable!("bulk-transfer handler received another data-packet kind")
    };
    let pending = PendingBulk {
        usbredir_id: id,
        endpoint,
        stream_id,
    };

    // qemu-rdp does not advertise usb_redir_cap_bulk_streams. Silently
    // treating a non-zero stream as stream zero would send data to the wrong
    // logical stream if a peer violated capability negotiation.
    if stream_id != 0 {
        tracing::warn!(
            id,
            ?endpoint,
            stream_id,
            "Rejected unsupported USB bulk stream"
        );
        return Ok(vec![pending.response(Status::Inval, 0, Bytes::new())]);
    }

    let pipe_handle = match active_device.bulk_pipe_handle(endpoint) {
        Ok(pipe_handle) => pipe_handle,
        Err(error) => {
            tracing::warn!(id, ?endpoint, %error, "Could not resolve active RDPEUSB bulk pipe");
            return Ok(vec![pending.response(Status::IoError, 0, Bytes::new())]);
        }
    };
    let request = match build_bulk_request(endpoint, pipe_handle, length, data) {
        Ok(request) => request,
        Err(status) => {
            tracing::warn!(
                id,
                ?endpoint,
                length,
                ?status,
                "Rejected malformed usbredir bulk packet"
            );
            return Ok(vec![pending.response(status, 0, Bytes::new())]);
        }
    };

    // This await is only the RDPEUSB transport submission barrier. The device
    // completion arrives independently through RedirEvent and is dispatched by
    // the outer SessionTask select loop.
    let sent = match request {
        rdpeusb::TransferRequest::In(request) => io.handle().transfer_in_request(request).await,
        rdpeusb::TransferRequest::Out(request) => io.handle().transfer_out_request(request).await,
    };
    let sent = match sent {
        Ok(sent) => sent,
        Err(error) => {
            tracing::warn!(id, ?endpoint, %error, "Failed to submit RDPEUSB bulk transfer");
            return Ok(vec![pending.response(Status::IoError, 0, Bytes::new())]);
        }
    };
    if !sent.expects_completion {
        tracing::error!(
            id,
            ?endpoint,
            request_id = sent.request_id,
            "RDPEUSB bulk transfer unexpectedly has no completion"
        );
        return Ok(vec![pending.response(Status::IoError, 0, Bytes::new())]);
    }
    pending_transfers.insert(sent.request_id, PendingTransfer::Bulk(pending))?;

    Ok(Vec::new())
}

fn build_bulk_request(
    endpoint: Endpoint,
    pipe_handle: PipeHandle,
    length: u32,
    data: Bytes,
) -> Result<rdpeusb::TransferRequest, Status> {
    if endpoint.is_input() {
        if !data.is_empty() {
            return Err(Status::Inval);
        }
        Ok(rdpeusb::TransferRequest::In(rdpeusb::bulk_or_interrupt_in(
            pipe_handle,
            length,
        )))
    } else {
        if usize::try_from(length) != Ok(data.len()) {
            return Err(Status::Inval);
        }
        Ok(rdpeusb::TransferRequest::Out(
            rdpeusb::bulk_or_interrupt_out(pipe_handle, data.to_vec()),
        ))
    }
}

pub(super) async fn handle_rdp_event(
    active_device: &mut ActiveDevice,
    event: RedirEvent,
    _io: &mut RdpeusbDeviceIo,
    pending: &mut PendingTransfers,
) -> anyhow::Result<Vec<Packet>> {
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
        }) => {
            if let Some(pending) = pending.remove(request_id) {
                let packet = match pending {
                    PendingTransfer::Control(pending) => {
                        control_transfer::complete(active_device, pending, kind)
                    }
                    PendingTransfer::Bulk(pending) => {
                        let direction_matches = matches!(
                            (&kind, pending.endpoint.is_input()),
                            (RdpeusbCompletionKind::TransferIn(_), true)
                                | (RdpeusbCompletionKind::TransferOut(_), false)
                        );
                        if !direction_matches {
                            tracing::error!(
                                channel_id,
                                request_id,
                                endpoint = ?pending.endpoint,
                                completion_kind = kind.name(),
                                "RDPEUSB completion direction does not match usbredir bulk request"
                            );
                        }
                        complete_bulk(pending, kind)
                    }
                };
                return Ok(vec![packet]);
            }

            match kind {
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
                    tracing::warn!(
                        channel_id,
                        request_id,
                        hresult = completion.hresult,
                        usbd_status = completion.ts_urb_result.header.usbd_status,
                        output_buffer_len = completion.output_buffer.len(),
                        ?device_instance_id,
                        "Unmatched RDPEUSB transfer-in completion"
                    );
                }
                RdpeusbCompletionKind::TransferOut(completion) => {
                    tracing::warn!(
                        channel_id,
                        request_id,
                        hresult = completion.hresult,
                        usbd_status = completion.ts_urb_result.header.usbd_status,
                        output_buffer_size = completion.output_buffer_size,
                        ?device_instance_id,
                        "Unmatched RDPEUSB transfer-out completion"
                    );
                }
            }
        }
    }
    Ok(Vec::new())
}

fn complete_bulk(pending: PendingBulk, completion: RdpeusbCompletionKind) -> Packet {
    match completion {
        RdpeusbCompletionKind::TransferIn(completion) if pending.endpoint.is_input() => {
            let status = completion_status(
                completion.hresult,
                completion.ts_urb_result.header.usbd_status,
            );
            if status != Status::Success {
                return pending.response(status, 0, Bytes::new());
            }

            let Ok(length) = u32::try_from(completion.output_buffer.len()) else {
                return pending.response(Status::IoError, 0, Bytes::new());
            };
            pending.response(
                Status::Success,
                length,
                Bytes::from(completion.output_buffer),
            )
        }
        RdpeusbCompletionKind::TransferOut(completion) if !pending.endpoint.is_input() => {
            let status = completion_status(
                completion.hresult,
                completion.ts_urb_result.header.usbd_status,
            );
            if status != Status::Success {
                return pending.response(status, 0, Bytes::new());
            }
            pending.response(Status::Success, completion.output_buffer_size, Bytes::new())
        }
        _ => pending.response(Status::IoError, 0, Bytes::new()),
    }
}

pub(super) fn completion_status(hresult: u32, usbd_status: u32) -> Status {
    if hresult != 0 {
        return Status::IoError;
    }

    match usbd_status {
        0 => Status::Success,
        USBD_STATUS_CANCELED => Status::Cancelled,
        USBD_STATUS_STALL_PID => Status::Stall,
        USBD_STATUS_TIMEOUT => Status::Timeout,
        USBD_STATUS_BABBLE_DETECTED => Status::Babble,
        _ => Status::IoError,
    }
}

impl PendingTransfers {
    fn insert(&mut self, request_id: RequestId, transfer: PendingTransfer) -> anyhow::Result<()> {
        let usbredir_id = transfer.usbredir_id();
        anyhow::ensure!(
            !self.by_rdpeusb.contains_key(&request_id),
            "RDPEUSB request ID {request_id} is already correlated with a usbredir data packet"
        );
        anyhow::ensure!(
            !self.by_usbredir.contains_key(&usbredir_id),
            "usbredir data packet ID {usbredir_id} is already pending"
        );

        self.by_rdpeusb.insert(
            request_id,
            PendingEntry {
                transfer,
                cancel_requested: false,
            },
        );
        self.by_usbredir.insert(usbredir_id, request_id);
        Ok(())
    }

    pub(super) fn insert_control(
        &mut self,
        request_id: RequestId,
        pending: control_transfer::PendingControl,
    ) -> anyhow::Result<()> {
        self.insert(request_id, PendingTransfer::Control(pending))
    }

    fn remove(&mut self, request_id: RequestId) -> Option<PendingTransfer> {
        let pending = self.by_rdpeusb.remove(&request_id)?;
        let usbredir_id = pending.transfer.usbredir_id();
        let indexed_request_id = self
            .by_usbredir
            .remove(&usbredir_id)
            .expect("pending RDPEUSB request must have a usbredir reverse index");
        debug_assert_eq!(indexed_request_id, request_id);
        Some(pending.transfer)
    }

    fn uncancelled_request_id(&self, usbredir_id: u64) -> Option<RequestId> {
        let request_id = *self.by_usbredir.get(&usbredir_id)?;
        let pending = self
            .by_rdpeusb
            .get(&request_id)
            .expect("usbredir reverse index must refer to a pending RDPEUSB request");
        (!pending.cancel_requested).then_some(request_id)
    }

    fn mark_cancel_requested(&mut self, request_id: RequestId) {
        let pending = self
            .by_rdpeusb
            .get_mut(&request_id)
            .expect("cancel target came from the pending RDPEUSB request map");
        pending.cancel_requested = true;
    }
}

impl PendingTransfer {
    fn usbredir_id(&self) -> u64 {
        match self {
            Self::Control(pending) => pending.usbredir_id(),
            Self::Bulk(pending) => pending.usbredir_id,
        }
    }
}

pub(super) fn cancel_data_packet(
    usbredir_id: u64,
    io: &RdpeusbDeviceIo,
    pending: &mut PendingTransfers,
) -> anyhow::Result<()> {
    let Some(request_id) = pending.uncancelled_request_id(usbredir_id) else {
        tracing::trace!(
            usbredir_id,
            "Ignored cancellation for an unknown, completed, or already-canceling usbredir packet"
        );
        return Ok(());
    };

    // The pending entry remains until the normal RDPEUSB completion arrives.
    // It resolves the race between a completed request and a successful cancel
    // and supplies the final usbredir status in both cases.
    io.cancel(request_id)?;
    pending.mark_cancel_requested(request_id);
    tracing::trace!(
        usbredir_id,
        request_id,
        "Queued RDPEUSB transfer cancellation"
    );
    Ok(())
}

impl PendingBulk {
    fn response(&self, status: Status, length: u32, data: Bytes) -> Packet {
        Packet::bulk_packet(
            self.usbredir_id,
            self.endpoint,
            status,
            length,
            self.stream_id,
            data,
        )
    }
}
