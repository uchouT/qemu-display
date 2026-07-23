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

mod proto;
mod rdpeusb_io;
mod session_task;

pub struct QemuUsbFactory {
    usbredir: UsbRedir<RdpUsb>,
}

impl QemuUsbFactory {
    pub async fn connect(display: &qemu_display::Display<'_>) -> anyhow::Result<Self> {
        let chardevs = display.usbredir_chardevs().await;
        tracing::debug!(
            chardev_count = chardevs.len(),
            "discovered QEMU usbredir chardevs"
        );
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
    DeviceText(DeviceText),
    Completion(RdpeusbCompletion),
}

#[derive(Debug)]
struct RdpeusbCompletion {
    channel_id: u32,
    request_id: RequestId,
    kind: RdpeusbCompletionKind,
}

#[derive(Debug)]
enum RdpeusbCompletionKind {
    IoControl(IoControlCompletionResult),
    InternalIoControl(IoControlCompletionResult),
    TransferIn(TransferInCompletionResult),
    TransferOut(TransferOutCompletionResult),
}

impl RedirEvent {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::DeviceText(_) => "device-text",
            Self::Completion(completion) => completion.kind.name(),
        }
    }

    fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::DeviceText(_) => None,
            Self::Completion(completion) => Some(completion.request_id),
        }
    }
}

impl RdpeusbCompletionKind {
    fn name(&self) -> &'static str {
        match self {
            Self::IoControl(_) => "io-control",
            Self::InternalIoControl(_) => "internal-io-control",
            Self::TransferIn(_) => "transfer-in",
            Self::TransferOut(_) => "transfer-out",
        }
    }
}

impl RdpUsbRedirDeviceBackend {
    fn new(usbredir: UsbRedir<RdpUsb>, rdp_handle: UsbDeviceHandle) -> Self {
        Self {
            usbredir,
            rdp_handle,
            sender: None,
        }
    }

    fn send_event(&self, event: RedirEvent) {
        let event_kind = event.kind_name();
        let request_id = event.request_id();
        let Some(sender) = &self.sender else {
            tracing::warn!(
                event_kind,
                ?request_id,
                "received an RDPEUSB event before the usbredir device session started"
            );
            return;
        };

        if sender.send(event).is_err() {
            tracing::trace!(
                event_kind,
                ?request_id,
                "discarded an RDPEUSB event after the usbredir device session stopped"
            );
        }
    }
}

struct RedirTask {
    usbredir: UsbRedir<RdpUsb>,
    rdp_handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
}

impl RedirTask {
    fn new(
        usbredir: UsbRedir<RdpUsb>,
        rdp_handle: UsbDeviceHandle,
        rx: UnboundedReceiver<RedirEvent>,
    ) -> Self {
        Self {
            usbredir,
            rdp_handle,
            rx,
        }
    }

    async fn start(self, announce: DeviceAnnounce) -> anyhow::Result<()> {
        let key = announce.device_instance_id.clone();
        let device = RdpUsbRedirDevice::new(announce, self.rdp_handle, self.rx);

        tracing::debug!(%key, "connecting RDPEUSB device to QEMU usbredir chardev");
        let connected = self.usbredir.connect_device(key.clone(), device).await?;
        tracing::debug!(%key, connected, "QEMU usbredir device connection completed");
        Ok(())
    }
}

impl UrbdrcDeviceServerBackend for RdpUsbRedirDeviceBackend {
    fn add_device(&mut self, device: DeviceAnnounce) -> ironrdp::pdu::PduResult<()> {
        if self.sender.is_some() {
            tracing::debug!("ignoring duplicate RDPEUSB device announcement");
            return Ok(());
        }
        tracing::debug!(
            device_instance_id = %device.device_instance_id,
            "starting usbredir device session"
        );
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
        self.send_event(RedirEvent::DeviceText(device_text));
    }

    fn io_control_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: IoControlCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        self.send_event(RedirEvent::Completion(RdpeusbCompletion {
            channel_id,
            request_id,
            kind: RdpeusbCompletionKind::IoControl(completion),
        }));
        Ok(())
    }

    fn internal_io_control_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: IoControlCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        self.send_event(RedirEvent::Completion(RdpeusbCompletion {
            channel_id,
            request_id,
            kind: RdpeusbCompletionKind::InternalIoControl(completion),
        }));
        Ok(())
    }

    fn transfer_in_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: TransferInCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        self.send_event(RedirEvent::Completion(RdpeusbCompletion {
            channel_id,
            request_id,
            kind: RdpeusbCompletionKind::TransferIn(completion),
        }));
        Ok(())
    }

    fn transfer_out_completed(
        &mut self,
        channel_id: u32,
        request_id: RequestId,
        completion: TransferOutCompletionResult,
    ) -> ironrdp::pdu::PduResult<()> {
        self.send_event(RedirEvent::Completion(RdpeusbCompletion {
            channel_id,
            request_id,
            kind: RdpeusbCompletionKind::TransferOut(completion),
        }));
        Ok(())
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
    announce: DeviceAnnounce,
    rdp_handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
}

impl RdpUsbRedirDevice {
    fn new(
        announce: DeviceAnnounce,
        rdp_handle: UsbDeviceHandle,
        rx: UnboundedReceiver<RedirEvent>,
    ) -> Self {
        Self {
            announce,
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
                tracing::error!(?error, "USB redirection session failed");
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
