use std::os::unix::net::UnixStream;

use anyhow::Context as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use usbredir_proto::{Cap, Event, Host, Packet, Parser, ParserConfig};

use super::{proto, redir_task::DeviceAnnouncement, RdpUsbRedirDevice};

const READ_BUFFER_SIZE: usize = 64 * 1024;

pub(super) struct SessionTask {
    stream: tokio::net::UnixStream,
    parser: Parser<Host>,
    announcement: Option<DeviceAnnouncement>,
    bridge: proto::Bridge,
    rx: tokio::sync::mpsc::UnboundedReceiver<super::RedirEvent>,
}

impl SessionTask {
    pub(super) fn new(device: RdpUsbRedirDevice, stream: UnixStream) -> qemu_display::Result<Self> {
        stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(stream)?;

        let RdpUsbRedirDevice {
            data,
            rdp_handle,
            rx,
        } = device;

        let parser = Parser::new(
            ParserConfig::new(concat!("qemu-rdp ", env!("CARGO_PKG_VERSION")))
                .cap(Cap::ConnectDeviceVersion)
                .cap(Cap::EpInfoMaxPacketSize)
                .cap(Cap::Ids64Bits)
                .cap(Cap::BulkLength32Bits),
        );

        Ok(Self {
            stream,
            parser,
            announcement: Some(data.device),
            bridge: proto::Bridge::new(rdp_handle, data.configuration),
            rx,
        })
    }

    pub(super) async fn run(mut self) -> anyhow::Result<()> {
        tracing::debug!("QEMU usbredir session task started");
        let written = self.flush_output().await?;
        tracing::debug!(written, "sent local usbredir Hello");

        let mut buffer = vec![0; READ_BUFFER_SIZE];
        loop {
            tokio::select! {
                read = self.stream.read(&mut buffer) => {
                    let read = read.context("read QEMU usbredir chardev")?;
                    if read == 0 {
                        tracing::debug!("QEMU usbredir chardev closed");
                        return Ok(());
                    }

                    tracing::trace!(read, "received bytes from QEMU usbredir chardev");
                    self.parser
                        .feed(&buffer[..read])
                        .context("parse QEMU usbredir data")?;
                    self.handle_parser_events().await?;
                }
                event = self.rx.recv() => {
                    let Some(event) = event else {
                        tracing::debug!("RDPEUSB device event channel closed");
                        return Ok(());
                    };

                    if let Some(packet) = self.bridge.handle_rdp_event(event)? {
                        self.parser
                            .send(&packet)
                            .context("encode usbredir response")?;
                    }
                }
            }

            let written = self.flush_output().await?;
            if written != 0 {
                tracing::debug!(written, "sent queued usbredir packets to QEMU");
            }
        }
    }

    async fn handle_parser_events(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.parser.poll() {
            match event {
                Event::Packet(packet) => match *packet {
                    Packet::Hello { version, caps } => {
                        tracing::debug!(%version, ?caps, "received QEMU usbredir Hello");
                        self.announce_device()?;
                    }
                    packet => self.bridge.handle_usbredir_packet(packet).await?,
                },
                Event::Error(error) => return Err(error).context("decode usbredir packet"),
            }
        }

        Ok(())
    }

    fn announce_device(&mut self) -> anyhow::Result<()> {
        let Some(announcement) = self.announcement.take() else {
            anyhow::bail!("received a second usbredir Hello");
        };

        // usbredir requires endpoint and interface metadata before the device
        // becomes visible to the guest.
        self.parser
            .send(&Packet::ep_info(announcement.ep_info))
            .context("encode usbredir endpoint information")?;
        self.parser
            .send(&Packet::interface_info(announcement.interface_info))
            .context("encode usbredir interface information")?;
        self.parser
            .send(&Packet::device_connect(announcement.device_connect))
            .context("encode usbredir device connection")?;
        tracing::debug!("queued usbredir EpInfo, InterfaceInfo, and DeviceConnect announcements");

        Ok(())
    }

    async fn flush_output(&mut self) -> anyhow::Result<usize> {
        let mut written = 0;
        while let Some(bytes) = self.parser.drain() {
            self.stream
                .write_all(&bytes)
                .await
                .context("write QEMU usbredir chardev")?;
            written += bytes.len();
        }

        Ok(written)
    }
}
