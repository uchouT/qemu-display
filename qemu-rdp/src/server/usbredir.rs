use std::os::unix::net::UnixStream;

use ironrdp_server::{DeviceFactory, DeviceText, RdpUsbDeviceAnnounceInfo, UsbRedirDevice};
use qemu_display::UsbRedir;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};

mod bridge;
mod data_inflight;
mod device;
mod session_task;
mod transfer;

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
    fn create_device(&mut self) -> Option<Box<dyn ironrdp_server::UsbRedirDevice>> {
        Some(Box::new(RdpUsbRedirDeviceBackend::new(
            self.usbredir.clone(),
        )))
    }
}

struct RdpUsbRedirDeviceBackend {
    usbredir: UsbRedir<RdpUsb>,
    sender: Option<UnboundedSender<RedirEvent>>,
}

/// Usbredir device session event.
#[derive(Debug)]
enum RedirEvent {
    DeviceText(DeviceText),
    Close,
}

impl RedirEvent {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::DeviceText(_) => "device-text",
            Self::Close => "close",
        }
    }
}

impl RdpUsbRedirDeviceBackend {
    fn new(usbredir: UsbRedir<RdpUsb>) -> Self {
        Self {
            usbredir,
            sender: None,
        }
    }

    fn send_event(&self, event: RedirEvent) {
        let event_kind = event.kind_name();
        let Some(sender) = &self.sender else {
            tracing::warn!(
                event_kind,
                "received an RDPEUSB event before the usbredir device session started"
            );
            return;
        };

        if sender.send(event).is_err() {
            tracing::trace!(
                event_kind,
                "discarded an RDPEUSB event after the usbredir device session stopped"
            );
        }
    }
}

impl UsbRedirDevice for RdpUsbRedirDeviceBackend {
    fn device_added(&mut self, info: RdpUsbDeviceAnnounceInfo) {
        if self.sender.is_some() {
            tracing::debug!("ignoring duplicate RDPEUSB device announcement");
            // TODO: make this trait method return Result
            return;
        }
        tracing::debug!(
            device_instance_id = %info.announce.device_instance_id,
            "starting usbredir device session"
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.sender = Some(tx);
        let (end_tx, end_rx) = oneshot::channel();

        let device = RdpUsbRedirDevice::new(info, rx, end_tx);
        let key = device.key().to_string();
        let usbredir = self.usbredir.clone();

        tokio::spawn(async move {
            tracing::debug!(%key, "connecting RDPEUSB device to QEMU usbredir chardev");
            if let Err(error) = usbredir.connect_device(key.clone(), device).await {
                tracing::error!(%key, ?error, "USB redirection connect task failed");
            }

            // Wakes once the session task drops its sender end: normal exit,
            // failure, and abort alike. A failed connect above has already
            // dropped it.
            let _ = end_rx.await;
            match usbredir.disconnect_device(&key).await {
                Ok(_) => tracing::debug!(%key, "released usbredir device session"),
                Err(error) => {
                    tracing::warn!(%key, ?error, "failed to disconnect usbredir device")
                }
            }
        });
    }

    fn device_text(&mut self, device_text: DeviceText) {
        self.send_event(RedirEvent::DeviceText(device_text));
    }

    fn close(&mut self) {
        if self.sender.is_none() {
            // Legal path: the device channel died before ADD_DEVICE arrived,
            // so no usbredir session was ever started.
            tracing::debug!("RDPEUSB device closed before a usbredir session started");
            return;
        }
        self.send_event(RedirEvent::Close);
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
    info: RdpUsbDeviceAnnounceInfo,
    rx: UnboundedReceiver<RedirEvent>,
    /// Dropped when the session task ends, waking the supervisor spawned in
    /// `device_added` to release the chardev slot.
    end_tx: oneshot::Sender<()>,
}

impl RdpUsbRedirDevice {
    fn new(
        info: RdpUsbDeviceAnnounceInfo,
        rx: UnboundedReceiver<RedirEvent>,
        end_tx: oneshot::Sender<()>,
    ) -> Self {
        Self { info, rx, end_tx }
    }

    fn key(&self) -> &str {
        &self.info.announce.device_instance_id
    }
}

struct RdpUsbSession {
    task: tokio::task::JoinHandle<()>,
}

impl RdpUsbSession {
    async fn new(device: RdpUsbRedirDevice, stream: UnixStream) -> qemu_display::Result<Self> {
        let task = session_task::SessionTask::new(device, stream)?;
        let task = tokio::spawn(async move {
            if let Err(error) = task.run().await {
                tracing::error!(?error, "USB redirection session failed");
            }
        });

        Ok(Self { task })
    }
}

impl Drop for RdpUsbSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}
