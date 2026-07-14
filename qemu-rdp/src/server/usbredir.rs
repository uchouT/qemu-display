use std::os::unix::net::UnixStream;

use ironrdp_rdpeusb::{
    io::{
        DeviceAnnounce, DeviceText, IoControlCompletionResult, RequestId,
        TransferInCompletionResult, TransferOutCompletionResult,
    },
    server::UrbdrcDeviceServerBackend,
};
use ironrdp_server::{DeviceFactory, UsbDeviceHandle};
use qemu_display::UsbRedir;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::server::usbredir::redir_task::RedirTask;

mod proto;
mod redir_task;
mod session_task;

pub struct QemuUsbFactory {
    usbredir: UsbRedir<RdpUsb>,
}

impl QemuUsbFactory {
    pub async fn connect(display: &qemu_display::Display<'_>) -> anyhow::Result<Self> {
        let chardevs = display.usbredir_chardevs().await;
        Ok(Self {
            usbredir: UsbRedir::new(chardevs, RdpUsb),
        })
    }
}

impl DeviceFactory for QemuUsbFactory {
    fn create_device(
        &mut self,
        handle: UsbDeviceHandle,
    ) -> Option<Box<dyn UrbdrcDeviceServerBackend>> {
        Some(Box::new(RdpUsbRedirDeviceBackend::new(
            self.usbredir.clone(),
            handle,
        )))
    }
}

struct RdpUsbRedirDeviceBackend {
    usbredir: UsbRedir<RdpUsb>,
    rdp_handle: UsbDeviceHandle,
    sender: Option<UnboundedSender<RedirEvent>>,
}

#[derive(Debug)]
enum RedirEvent {
    TransferInCompleted {
        request_id: RequestId,
        completion: TransferInCompletionResult,
    },
}

impl RdpUsbRedirDeviceBackend {
    fn new(usbredir: UsbRedir<RdpUsb>, rdp_handle: UsbDeviceHandle) -> Self {
        Self {
            usbredir,
            rdp_handle,
            sender: None,
        }
    }
}

impl UrbdrcDeviceServerBackend for RdpUsbRedirDeviceBackend {
    fn add_device(&mut self, device: DeviceAnnounce) -> ironrdp::pdu::PduResult<()> {
        if self.sender.is_some() {
            return Ok(());
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.sender = Some(tx);
        let task = RedirTask::new(self.usbredir.clone(), self.rdp_handle.clone(), rx);
        tokio::spawn(async {
            if let Err(e) = task.start(device).await {
                tracing::error!("task failed: {e}")
            }
        });
        Ok(())
    }

    fn device_text(&mut self, device_text: DeviceText) {
        todo!("check the data, send to qemu unix stream")
    }

    fn io_control_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: IoControlCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        todo!("check the data, send to qemu unix stream")
    }

    fn internal_io_control_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: IoControlCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        todo!("check the data, send to qemu unix stream")
    }

    fn transfer_in_completed(
        &mut self,
        _channel_id: u32,
        request_id: RequestId,
        completion: TransferInCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        if let Some(sender) = &self.sender {
            let _ = sender.send(RedirEvent::TransferInCompleted {
                request_id,
                completion,
            });
        }
        Ok(())
    }

    fn transfer_out_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: TransferOutCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        todo!("check the data, send to qemu unix stream")
    }
}

#[derive(Clone, Copy)]
struct RdpUsb;

#[async_trait::async_trait]
impl qemu_display::UsbRedirBackend for RdpUsb {
    type SessionInput = RdpUsbRedirDevice;
    type Key = String;
    type Session = RdpUsbSession;
    async fn start_session(
        &self,
        input: Self::SessionInput,
        stream: UnixStream,
    ) -> qemu_display::Result<Self::Session> {
        RdpUsbSession::new(input, stream).await
    }
}

struct RdpUsbRedirDevice {
    data: redir_task::DeviceData,
    rdp_handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
}

impl RdpUsbRedirDevice {
    fn new(
        data: redir_task::DeviceData,
        rdp_handle: UsbDeviceHandle,
        rx: UnboundedReceiver<RedirEvent>,
    ) -> Self {
        Self {
            data,
            rdp_handle,
            rx,
        }
    }
}

struct RdpUsbSession {
    task: tokio::task::JoinHandle<()>,
    handle: UsbDeviceHandle,
}

impl RdpUsbSession {
    async fn new(device: RdpUsbRedirDevice, stream: UnixStream) -> qemu_display::Result<Self> {
        let handle = device.rdp_handle.clone();
        let task = session_task::SessionTask::new(device, stream)?;
        let task = tokio::spawn(async move {
            if let Err(error) = task.run().await {
                tracing::error!(%error, "USB redirection session failed");
            }
        });

        Ok(Self { task, handle })
    }
}

impl Drop for RdpUsbSession {
    fn drop(&mut self) {
        // TODO: handle session close
        //
        // let _ = self.handle.retract_request(reason);
        self.task.abort();
    }
}
