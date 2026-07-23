use std::{collections::VecDeque, time::Duration};

use anyhow::{bail, ensure, Context as _};
use ironrdp_rdpeusb::io::{
    InternalIoControlPacket, IoControlCompletionResult, IoControlPacket, RequestId,
    TransferInCompletionResult, TransferInPacket, TransferOutCompletionResult, TransferOutPacket,
};
use ironrdp_server::UsbDeviceHandle;
use tokio::sync::mpsc::UnboundedReceiver;

use super::{RdpeusbCompletion, RdpeusbCompletionKind, RedirEvent};

const RDPEUSB_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-device RDPEUSB I/O endpoint.
///
/// This is the only owner of the device event receiver. The session loop calls
/// [`Self::recv`] normally, while synchronous operations temporarily consume
/// the same receiver through methods such as [`Self::wait_transfer_in`].
pub(super) struct RdpeusbDeviceIo {
    handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
    deferred: VecDeque<RedirEvent>,
}

impl RdpeusbDeviceIo {
    pub(super) fn new(handle: UsbDeviceHandle, rx: UnboundedReceiver<RedirEvent>) -> Self {
        Self {
            handle,
            rx,
            deferred: VecDeque::new(),
        }
    }

    /// Receives the next event available to the outer session loop.
    pub(super) async fn recv(&mut self) -> Option<RedirEvent> {
        if let Some(event) = self.deferred.pop_front() {
            return Some(event);
        }

        self.rx.recv().await
    }

    /// Per USB redirection protocol v0.7: Control packets are handled synchroneously inside the
    /// usb-host, it will hand the request over to the host os and then *wait* for a response. The
    /// usb-host will thus stop processing further packets.
    ///
    /// Waits for one request's completion while preserving unrelated events.
    ///
    /// The deferred queue is searched first because another synchronous wait may already have
    /// received and parked this completion.
    async fn wait_completion(
        &mut self,
        request_id: RequestId,
    ) -> anyhow::Result<RdpeusbCompletion> {
        if let Some(completion) = take_deferred_completion(&mut self.deferred, request_id) {
            return Ok(completion);
        }

        tokio::time::timeout(RDPEUSB_COMPLETION_TIMEOUT, async {
            loop {
                let event = self.rx.recv().await.ok_or_else(|| {
                    anyhow::anyhow!(
                        "USB device session dropped while waiting for RDPEUSB request {request_id}"
                    )
                })?;

                match event {
                    RedirEvent::Completion(completion) if completion.request_id == request_id => {
                        return Ok(completion);
                    }
                    event => {
                        tracing::trace!(
                            waiting_request_id = request_id,
                            deferred_request_id = ?event.request_id(),
                            event_kind = event.kind_name(),
                            "deferred an unrelated RDPEUSB event"
                        );
                        self.deferred.push_back(event);
                    }
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "timed out after {:?} waiting for RDPEUSB request {request_id} completion",
                RDPEUSB_COMPLETION_TIMEOUT
            )
        })?
    }

    /// Sends a transfer-in request and waits for its complete RDPEUSB result.
    pub(super) async fn wait_transfer_in(
        &mut self,
        packet: TransferInPacket,
    ) -> anyhow::Result<TransferInCompletionResult> {
        let sent = self.handle.transfer_in_request(packet).await?;
        ensure!(
            sent.expects_completion,
            "RDPEUSB transfer-in unexpectedly has no completion"
        );

        let completion = self.wait_completion(sent.request_id).await?;
        match completion.kind {
            RdpeusbCompletionKind::TransferIn(completion) => Ok(completion),
            kind => bail!(
                "RDPEUSB request {} expected transfer-in completion, received {}",
                sent.request_id,
                kind.name()
            ),
        }
    }

    /// Sends a transfer-out request and waits for its complete RDPEUSB result.
    ///
    /// `None` is returned for a valid no-ack isochronous transfer, for which
    /// RDPEUSB deliberately sends no completion.
    pub(super) async fn wait_transfer_out(
        &mut self,
        packet: TransferOutPacket,
    ) -> anyhow::Result<Option<TransferOutCompletionResult>> {
        let sent = self.handle.transfer_out_request(packet).await?;
        if !sent.expects_completion {
            return Ok(None);
        }

        let completion = self.wait_completion(sent.request_id).await?;
        match completion.kind {
            RdpeusbCompletionKind::TransferOut(completion) => Ok(Some(completion)),
            kind => bail!(
                "RDPEUSB request {} expected transfer-out completion, received {}",
                sent.request_id,
                kind.name()
            ),
        }
    }

    /// Sends an I/O-control request and waits for its complete result.
    pub(super) async fn wait_io_control(
        &mut self,
        packet: IoControlPacket,
    ) -> anyhow::Result<IoControlCompletionResult> {
        let sent = self.handle.ioctl_request(packet).await?;
        ensure!(
            sent.expects_completion,
            "RDPEUSB I/O control unexpectedly has no completion"
        );

        let completion = self.wait_completion(sent.request_id).await?;
        match completion.kind {
            RdpeusbCompletionKind::IoControl(completion) => Ok(completion),
            kind => bail!(
                "RDPEUSB request {} expected I/O-control completion, received {}",
                sent.request_id,
                kind.name()
            ),
        }
    }

    /// Sends an internal-I/O-control request and waits for its complete result.
    pub(super) async fn wait_internal_io_control(
        &mut self,
        packet: InternalIoControlPacket,
    ) -> anyhow::Result<IoControlCompletionResult> {
        let sent = self.handle.internal_ioctl_request(packet).await?;
        ensure!(
            sent.expects_completion,
            "RDPEUSB internal I/O control unexpectedly has no completion"
        );

        let completion = self.wait_completion(sent.request_id).await?;
        match completion.kind {
            RdpeusbCompletionKind::InternalIoControl(completion) => Ok(completion),
            kind => bail!(
                "RDPEUSB request {} expected internal-I/O-control completion, received {}",
                sent.request_id,
                kind.name()
            ),
        }
    }

    /// Queues an RDPEUSB cancel request.
    ///
    /// This is fire-and-forget: success only means the server event was
    /// accepted locally, not that the cancel PDU was written or acknowledged.
    /// A canceled request does not produce a completion, so the bridge must
    /// finish its corresponding usbredir request locally.
    ///
    /// IronRDP also needs to retire the server-side pending entry when it
    /// sends the cancel; the qemu-rdp layer cannot repair that bookkeeping.
    pub(super) fn cancel(&self, request_id: RequestId) -> anyhow::Result<()> {
        self.handle.cancel_request(request_id)
    }

    /// Queues a device-text query.
    ///
    /// RDPEUSB does not attach a request ID to its response, so the resulting
    /// [`RedirEvent::DeviceText`] is handled only by the outer event loop.
    /// Callers must serialize queries if response association matters.
    pub(super) fn query_device_text(&self, text_type: u32, locale_id: u32) -> anyhow::Result<()> {
        self.handle.query_device_text_request(text_type, locale_id)
    }

    /// Isochronous RDPEUSB device handle.
    pub(super) fn handle(&self) -> &UsbDeviceHandle {
        &self.handle
    }
}

fn take_deferred_completion(
    deferred: &mut VecDeque<RedirEvent>,
    request_id: RequestId,
) -> Option<RdpeusbCompletion> {
    let index = deferred
        .iter()
        .position(|event| event.request_id() == Some(request_id))?;
    let event = deferred
        .remove(index)
        .expect("deferred completion index came from the same queue");
    let RedirEvent::Completion(completion) = event else {
        unreachable!("only completion events carry request IDs");
    };
    Some(completion)
}
