use std::os::unix::net::UnixStream;

use anyhow::Context as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use usbredir_proto::{Cap, Event, Host, Parser, ParserConfig};

use super::{proto, rdpeusb_io::RdpeusbDeviceIo, RdpUsbRedirDevice};

const READ_BUFFER_SIZE: usize = 64 * 1024;

pub(super) struct SessionTask {
    stream: tokio::net::UnixStream,
    parser: Parser<Host>,
    bridge: proto::Bridge,
    io: RdpeusbDeviceIo,
}

impl SessionTask {
    pub(super) fn new(device: RdpUsbRedirDevice, stream: UnixStream) -> qemu_display::Result<Self> {
        stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(stream)?;

        let RdpUsbRedirDevice {
            announce,
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
            bridge: proto::Bridge::new(announce),
            io: RdpeusbDeviceIo::new(rdp_handle, rx),
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
                event = self.io.recv() => {
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
            };

            let written = self.flush_output().await?;
            if written != 0 {
                tracing::debug!(written, "sent queued usbredir packets to QEMU");
            }
        }
    }

    async fn handle_parser_events(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.parser.poll() {
            match event {
                Event::Packet(packet) => {
                    let packets = self
                        .bridge
                        .handle_usbredir_packet(*packet, &mut self.io)
                        .await?;
                    for packet in packets {
                        tracing::debug!(packet = ?packet, "send usbredir packet to QEMU");
                        self.parser
                            .send(&packet)
                            .context("encode usbredir response")?;
                    }

                    // Complete any response barrier before dispatching another
                    // packet already queued by the sans-I/O parser.
                    self.flush_output().await?;
                }
                Event::Error(error) => return Err(error).context("decode usbredir packet"),
            }
        }

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
