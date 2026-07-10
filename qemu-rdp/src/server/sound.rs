use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Result;
// use tracing::{debug, error, warn};
use ironrdp::{
    rdpsnd::{
        pdu,
        server::{NegotiatedFormat, RdpsndError},
    },
    server::{
        RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender,
        SoundServerFactory,
    },
};
use tokio::sync::mpsc;

use qemu_display::{Audio, AudioOutHandler, Display, PCMInfo, Volume};
use tracing::{debug, trace, warn};

#[derive(Debug, PartialEq)]
enum State {
    Init,
    Ready(u64, pdu::AudioFormat),
}

#[derive(Debug)]
pub struct Inner {
    state: State,
    dbus_nsamples: Option<u32>,
    start_time: Instant,
    ev_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    client_fmt: Option<pdu::AudioFormat>,
    volume: Option<Volume>,
    opus_enc: opus::Encoder,
    _audio: Option<Audio>,
}

#[derive(Debug)]
pub(crate) struct DBusHandler {
    inner: Arc<Mutex<Inner>>,
}

fn wave_format_from(info: &PCMInfo) -> Result<pdu::AudioFormat, &str> {
    if info.be {
        return Err("unsupported big-endian audio format");
    }

    if info.is_float {
        return Err("unsupported float audio format");
    }

    let n_block_align = match info.bytes_per_frame.try_into() {
        Ok(bpf) => bpf,
        Err(_) => {
            return Err("unsupported audio bytes_per_frame");
        }
    };

    Ok(pdu::AudioFormat {
        format: pdu::WaveFormat::PCM,
        n_channels: info.nchannels.into(),
        n_samples_per_sec: info.freq,
        n_avg_bytes_per_sec: info.bytes_per_second,
        n_block_align,
        bits_per_sample: info.bits.into(),
        data: None,
    })
}

#[async_trait::async_trait]
impl AudioOutHandler for DBusHandler {
    async fn init(&mut self, id: u64, info: PCMInfo) {
        let mut inner = self.inner.lock().unwrap();

        if inner.state != State::Init {
            warn!("audio already initialized, no mixing yet");
            return;
        }
        let format = match wave_format_from(&info) {
            Ok(fmt) => fmt,
            Err(err) => {
                warn!("{}", err);
                return;
            }
        };

        inner.state = State::Ready(id, format);
        debug!(?inner.state);
    }

    async fn fini(&mut self, id: u64) {
        let mut inner = self.inner.lock().unwrap();

        if matches!(inner.state, State::Ready(iid, _) if iid == id) {
            inner.state = State::Init;
            debug!(?inner.state);
        }
    }

    async fn set_enabled(&mut self, id: u64, enabled: bool) {
        debug!(?id, ?enabled)
    }

    async fn set_volume(&mut self, id: u64, volume: Volume) {
        debug!(?id, ?volume);

        let mut inner = self.inner.lock().unwrap();
        inner.volume = Some(volume.clone());

        if inner.client_fmt.is_none() {
            return;
        }

        let (left, right) = if volume.mute {
            (0, 0)
        } else {
            #[allow(clippy::get_first)]
            (
                volume.volume.get(0).copied().unwrap_or(0),
                volume.volume.get(1).copied().unwrap_or(0),
            )
        };

        let (left, right) = ((left as u16) * 257, (right as u16) * 257);

        if let Some(sender) = inner.ev_sender.as_ref() {
            let _ = sender.send(ServerEvent::Rdpsnd(RdpsndServerMessage::SetVolume {
                left,
                right,
            }));
        }
    }

    async fn write(&mut self, id: u64, mut data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();

        let State::Ready(iid, fmt) = &inner.state else {
            debug!("QEMU stream is not ready");
            return;
        };
        let Some(client_fmt) = &inner.client_fmt else {
            trace!("RDP client has not started and negotiated a compatible format");
            return;
        };
        if *iid != id {
            return;
        }
        // TODO: handle format conversion?
        if client_fmt.n_channels != fmt.n_channels
            || client_fmt.n_samples_per_sec != fmt.n_samples_per_sec
            || client_fmt.bits_per_sample != fmt.bits_per_sample
        {
            debug!("Incompatible client and server formats");
            return;
        }

        // TODO: apply volume if necessary

        if client_fmt.format == pdu::WaveFormat::OPUS {
            let input =
                unsafe { std::slice::from_raw_parts(data.as_ptr() as *const i16, data.len() / 2) };
            data = match inner.opus_enc.encode_vec(input, data.len()) {
                Ok(data) => data,
                Err(err) => {
                    warn!("Failed to encode with Opus: {}", err);
                    return;
                }
            }
        }

        if let Some(sender) = inner.ev_sender.as_ref() {
            let ts = inner.start_time.elapsed().as_millis() as _;
            let _ = sender.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
        }
    }
}

#[derive(Clone, Debug)]
pub struct SoundHandler {
    inner: Arc<Mutex<Inner>>,
}

impl ServerEventSender for SoundHandler {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        let mut inner = self.inner.lock().unwrap();

        inner.ev_sender = Some(sender);
    }
}

#[derive(Debug)]
pub(crate) struct RDPSndHandler {
    inner: Arc<Mutex<Inner>>,
}

const OPUS_48K: pdu::AudioFormat = pdu::AudioFormat {
    format: pdu::WaveFormat::OPUS,
    n_channels: 2,
    n_samples_per_sec: 48000,
    n_avg_bytes_per_sec: 192000,
    n_block_align: 4,
    bits_per_sample: 16,
    data: None,
};

const PCM_48K: pdu::AudioFormat = pdu::AudioFormat {
    format: pdu::WaveFormat::PCM,
    n_channels: 2,
    n_samples_per_sec: 48000,
    n_avg_bytes_per_sec: 192000,
    n_block_align: 4,
    bits_per_sample: 16,
    data: None,
};

impl RdpsndServerHandler for RDPSndHandler {
    fn get_formats(&self) -> &[pdu::AudioFormat] {
        let inner = self.inner.lock().unwrap();

        if let Some(nsamples) = &inner.dbus_nsamples {
            if [120u32, 240, 480, 960].contains(nsamples) {
                return &[OPUS_48K, PCM_48K];
            }
        }

        debug!("No Opus codec since DBus has incompatible nsamples");
        &[PCM_48K]
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> std::result::Result<(), Box<dyn RdpsndError>> {
        let mut inner = self.inner.lock().unwrap();
        inner.client_fmt = Some(format.format().clone());
        debug!(?inner.client_fmt);
        Ok(())
    }

    fn stop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.client_fmt = None;
    }
}

impl SoundServerFactory for SoundHandler {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(RDPSndHandler {
            inner: self.inner.clone(),
        })
    }
}

impl SoundHandler {
    pub async fn connect(display: &Display<'_>) -> Result<Self> {
        let mut audio =
            Audio::new(display.connection(), Some(display.destination().to_owned())).await?;
        let dbus_nsamples = audio.proxy.nsamples().await.ok();
        let inner = Arc::new(Mutex::new(Inner {
            start_time: Instant::now(),
            dbus_nsamples,
            state: State::Init,
            ev_sender: None,
            client_fmt: None,
            volume: None,
            opus_enc: opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio)?,
            _audio: None,
        }));

        // TODO: register only after connection?
        let handler = DBusHandler {
            inner: inner.clone(),
        };
        audio.register_out_listener(handler).await?;
        inner.lock().unwrap()._audio = Some(audio);

        Ok(Self { inner })
    }
}
