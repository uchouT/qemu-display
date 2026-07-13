use super::*;

pub(super) struct RedirTask {
    usbredir: UsbRedir<RdpUsb>,
    rdp_handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
}

// TODO: Data for creating usb_redir_device_connect
pub(super) struct DeviceData;

impl RedirTask {
    pub(super) fn new(
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

    async fn boot(&mut self, announce: DeviceAnnounce) -> anyhow::Result<DeviceData> {
        todo!()
    }

    /// Start a usbredit task, always starts from State::Boot
    pub(super) async fn start(mut self, announce: DeviceAnnounce) -> anyhow::Result<()> {
        // communicate with RDP client to get enough data for creating device via usbredir.
        let usbredir_device = loop {
            let event = self
                .rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("session dropped"))?;

            // TODO: ironrdp processor receives REGISTER_REQUEST_CALLBACK should emit a callback
            if !matches!(event, RedirEvent::Ready) {
                tracing::debug!("received {event:?} while booting");
                continue;
            }
            break self.boot(announce).await?;
        };

        // TODO: create device
        let device = RdpUsbRedirDevice {
            rdp_handle: self.rdp_handle,
            rx: self.rx,
        };

        self.usbredir
            .connect_device(DeviceKey::from(&device), device)
            .await?;
        Ok(())
    }
}
