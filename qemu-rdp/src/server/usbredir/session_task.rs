use std::os::unix::net::UnixStream;

use anyhow::Context as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use usbredir_proto::{Cap, Event, Host, Packet, Parser, ParserConfig};

use super::{bridge::Bridge, RdpUsbRedirDevice, RedirEvent};

const READ_BUFFER_SIZE: usize = 64 * 1024;

/// What one pass of the session loop decided about the session's lifetime.
enum Flow {
    Continue,
    /// QEMU closed the chardev, so nothing more can be written to it.
    PeerClosed,
    /// The redirected device is gone, which QEMU still has to be told.
    DeviceGone,
}

/// Transport half of one usbredir session.
///
/// It frames the QEMU chardev, drives the sans-I/O parser, and pumps the three
/// event sources of a live session. Everything it learns is handed to
/// [`Bridge`]; it holds no USB or usbredir state of its own.
pub(super) struct SessionTask {
    stream: tokio::net::UnixStream,
    parser: Parser<Host>,
    bridge: Bridge,
    redir_ev_rx: tokio::sync::mpsc::UnboundedReceiver<RedirEvent>,
    /// Reused response buffer: bridge handlers append, the transport drains.
    responses: Vec<Packet>,
    /// Dropped when the session ends, waking the device supervisor to release
    /// the chardev slot.
    _end_tx: tokio::sync::oneshot::Sender<()>,
}

impl SessionTask {
    pub(super) fn new(device: RdpUsbRedirDevice, stream: UnixStream) -> qemu_display::Result<Self> {
        stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(stream)?;

        let RdpUsbRedirDevice { info, rx, end_tx } = device;

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
            bridge: Bridge::new(info),
            redir_ev_rx: rx,
            responses: Vec::new(),
            _end_tx: end_tx,
        })
    }

    pub(super) async fn run(mut self) -> anyhow::Result<()> {
        tracing::debug!("QEMU usbredir session task started");
        let written = self.flush_output().await?;
        tracing::debug!(written, "sent local usbredir Hello");

        let mut buffer = vec![0; READ_BUFFER_SIZE];
        loop {
            // Every branch is cancel-safe, which `select!` requires: `read`
            // and `recv` by contract, and `next_completion` because a request
            // only leaves the in-flight set once its future has yielded.
            let flow = tokio::select! {
                read = self.stream.read(&mut buffer) => {
                    let read = read.context("read QEMU usbredir chardev")?;
                    if read == 0 {
                        tracing::debug!("QEMU usbredir chardev closed");
                        Flow::PeerClosed
                    } else {
                        tracing::trace!(read, "received bytes from QEMU usbredir chardev");
                        self.parser
                            .feed(&buffer[..read])
                            .context("parse QEMU usbredir data")?;
                        self.dispatch_parsed_packets().await?;
                        Flow::Continue
                    }
                }
                completed = self.bridge.next_completion() => {
                    self.bridge
                        .handle_completion(completed?, &mut self.responses)
                        .await?;
                    Flow::Continue
                }
                event = self.redir_ev_rx.recv() => {
                    // Losing the device is a transport fact, not a usbredir
                    // one: the requests still in flight can no longer be
                    // answered, and dropping `_end_tx` lets the supervisor
                    // release the chardev. What the bridge decides is only
                    // what QEMU is told about it.
                    match event {
                        None => {
                            tracing::debug!("RDPEUSB device event channel closed");
                            Flow::DeviceGone
                        }
                        Some(RedirEvent::Close) => {
                            tracing::debug!("RDPEUSB device channel closed");
                            Flow::DeviceGone
                        }
                        Some(RedirEvent::DeviceText(text)) => {
                            self.bridge
                                .handle_device_text(text, &mut self.responses)
                                .await?;
                            Flow::Continue
                        }
                    }
                }
            };

            // Nothing can be written to a chardev that is already gone, and
            // the queued responses have nowhere left to go.
            if matches!(flow, Flow::PeerClosed) {
                return Ok(());
            }
            if matches!(flow, Flow::DeviceGone) {
                self.bridge.handle_device_gone(&mut self.responses);
            }

            self.encode_responses()?;
            let written = self.flush_output().await?;
            if written != 0 {
                tracing::debug!(written, "sent queued usbredir packets to QEMU");
            }

            if matches!(flow, Flow::DeviceGone) {
                return Ok(());
            }
        }
    }

    async fn dispatch_parsed_packets(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.parser.poll() {
            match event {
                Event::Packet(packet) => {
                    self.bridge
                        .handle_usbredir_packet(*packet, &mut self.responses)
                        .await?;
                }
                Event::Error(error) => return Err(error).context("decode usbredir packet"),
            }
        }

        Ok(())
    }

    /// Moves the responses a handler appended into the parser's output queue.
    fn encode_responses(&mut self) -> anyhow::Result<()> {
        for packet in self.responses.drain(..) {
            tracing::debug!(packet = ?packet, "send usbredir packet to QEMU");
            self.parser
                .send(&packet)
                .context("encode usbredir response")?;
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
