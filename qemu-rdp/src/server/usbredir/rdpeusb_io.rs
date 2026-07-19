use std::collections::VecDeque;

use anyhow::ensure;
use ironrdp_rdpeusb::io::TransferInPacket;
use ironrdp_server::UsbDeviceHandle;
use tokio::sync::mpsc::UnboundedReceiver;

use super::RedirEvent;

/// Per-device RDPEUSB I/O endpoint.
///
/// This is the only owner of the device event receiver. The session loop uses
/// [`Self::recv`] while idle, and synchronous protocol operations temporarily
/// take over the same receiver through methods such as [`Self::transfer_in`].
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

    /// Receives the next event not consumed by a synchronous operation.
    pub(super) async fn recv(&mut self) -> Option<RedirEvent> {
        if let Some(event) = self.deferred.pop_front() {
            return Some(event);
        }

        self.rx.recv().await
    }

    /// Performs an RDPEUSB `TRANSFER_IN_REQUEST` and waits for its completion.
    ///
    /// Completions for other outstanding requests are preserved for the outer
    /// session event loop instead of being dropped.
    pub(super) async fn transfer_in(
        &mut self,
        packet: TransferInPacket,
    ) -> anyhow::Result<Vec<u8>> {
        let sent = self.handle.transfer_in_request(packet).await?;
        ensure!(
            sent.expects_completion,
            "RDPEUSB transfer-in unexpectedly has no completion"
        );

        loop {
            let event = self.rx.recv().await.ok_or_else(|| {
                anyhow::anyhow!(
                    "USB device session dropped while waiting for transfer-in completion"
                )
            })?;

            match event {
                RedirEvent::TransferInCompleted {
                    request_id,
                    completion,
                } if request_id == sent.request_id => {
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
                    return Ok(completion.output_buffer);
                }
                event => {
                    tracing::trace!(
                        request_id = sent.request_id,
                        ?event,
                        "deferred unrelated RDPEUSB event while waiting for transfer-in completion"
                    );
                    self.deferred.push_back(event);
                }
            }
        }
    }
}
