//! usbredir session semantics on top of the IronRDP USB device facade.

use anyhow::{ensure, Context as _};
use ironrdp_rdpeusb::pdu::sink::DeviceSpeed;
use ironrdp_server::{
    DeviceAnnounce, DeviceText, RdpUsbDeviceAnnounceInfo, UsbDeviceHandle, UsbRequestCompletion,
};
use ironrdp_usb::{descriptor::ConfigurationDescriptorSet, transfer::UsbError, InterfaceSelection};
use usbredir_proto::{Caps, Packet, RequestKind, RequestPacket, Speed, Status};

use super::data_inflight::{Completed, DataInFlight, Purpose};
use super::device::ActiveDevice;
use super::transfer;

/// Stateful boundary between usbredir packets and IronRDP device I/O.
///
/// The session task owns only transport framing and I/O. Protocol negotiation,
/// device state, packet translation, and request correlation belong here.
///
/// Handlers append their responses to a caller-owned buffer instead of
/// returning one: a data-path response then costs no allocation, and every
/// handler has the same shape.
pub(super) struct Bridge {
    usb: UsbDeviceHandle,
    inflight: DataInFlight,
    /// The device as RDPEUSB announced it, before any usbredir negotiation.
    announce: DeviceAnnounce,
    state: State,
}

/// Mutually exclusive phases of one usbredir bridge session.
///
/// Device preparation needs no phase of its own: packets are dispatched one at
/// a time, so no other packet can be observed while the peer Hello is still
/// being answered.
enum State {
    AwaitingPeerHello,
    Active(Box<ActiveDevice>),
}

impl Bridge {
    pub(super) fn new(info: RdpUsbDeviceAnnounceInfo) -> Self {
        let RdpUsbDeviceAnnounceInfo {
            announce,
            usb_handle,
        } = info;

        Self {
            usb: usb_handle,
            inflight: DataInFlight::new(),
            announce,
            state: State::AwaitingPeerHello,
        }
    }

    /// Waits for the next in-flight data request to end.
    ///
    /// Cancel-safe, so the session task can select on it: a request only
    /// leaves [`DataInFlight`] once its future has yielded, and nothing is
    /// awaited after that point.
    pub(super) async fn next_completion(&mut self) -> anyhow::Result<Completed> {
        self.inflight.completed().await
    }

    /// Translates one packet received from QEMU.
    pub(super) async fn handle_usbredir_packet(
        &mut self,
        packet: Packet,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        match packet {
            Packet::Hello { version, caps } => self.handle_peer_hello(&version, caps, out).await,
            Packet::Request(request) => self.handle_request(request, out).await,
            Packet::Data(packet) => {
                let Self {
                    usb,
                    inflight,
                    state,
                    ..
                } = self;
                let State::Active(device) = state else {
                    anyhow::bail!("received a usbredir data packet before peer Hello");
                };

                transfer::handle_data(usb, device, inflight, packet, out).await
            }
            packet => {
                tracing::warn!(?packet, "Ignored unexpected usbredir packet from QEMU");
                Ok(())
            }
        }
    }

    /// Dispatches a finished USB request by what it was submitted for.
    pub(super) async fn handle_completion(
        &mut self,
        completed: Completed,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        let State::Active(device) = &mut self.state else {
            anyhow::bail!("received a usb completion before peer Hello");
        };

        let Completed { purpose, result } = completed;
        match purpose {
            Purpose::Data(ctx) => transfer::complete(device, ctx, result, out),
        }
    }

    /// Tells QEMU that the redirected device is gone.
    ///
    /// The requests still in flight are not answered: the RDPEUSB channel that
    /// would carry their completions is exactly what went away. usbredir
    /// expects that, and a guest discards its pending packets when the device
    /// disconnects.
    ///
    /// Nothing is announced before the device was: QEMU cannot be told a device
    /// left if it was never told one arrived.
    ///
    /// This is the last thing a session does, so the bridge state is left as it
    /// is rather than given a phase no other packet can observe.
    pub(super) fn handle_device_gone(&self, out: &mut Vec<Packet>) {
        let State::Active(_) = &self.state else {
            tracing::debug!("RDPEUSB device ended before it was announced to QEMU");
            return;
        };

        out.push(Packet::DeviceDisconnect);
        tracing::debug!("Announcing usbredir device disconnect to QEMU");
    }

    /// Handles device text reported by the RDP client.
    ///
    /// usbredir has nowhere to put it: the device description QEMU shows comes
    /// from the descriptors. Nothing queries device text yet, so an arriving
    /// one is unsolicited.
    pub(super) async fn handle_device_text(
        &mut self,
        text: DeviceText,
        _out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        tracing::debug!(
            hresult = text.hresult,
            description_len = text.description.len(),
            "Ignored unsolicited RDPEUSB device text"
        );
        Ok(())
    }

    /// Answers the peer Hello by making the redirected device visible to QEMU.
    async fn handle_peer_hello(
        &mut self,
        version: &str,
        caps: Caps,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(self.state, State::AwaitingPeerHello),
            "received a second usbredir Hello"
        );

        tracing::debug!(version, peer_caps = %caps, "Received QEMU usbredir Hello");
        tracing::debug!(
            device_instance_id = %self.announce.device_instance_id,
            "preparing RDPEUSB device for usbredir announcement"
        );

        let device = ActiveDevice::prepare(&self.usb, self.device_speed())
            .await
            .context("prepare rdpeusb device for usbredir")?;

        push_metadata(&device, out);
        out.push(Packet::device_connect(device.device_connect()));

        self.state = State::Active(Box::new(device));
        Ok(())
    }

    /// Dispatches the usbredir requests that are answered by a status packet.
    ///
    /// These are the operations usbredir treats as synchronous, so each one is
    /// awaited inline: no other packet is dispatched until it has been
    /// answered.
    async fn handle_request(
        &mut self,
        request: RequestPacket,
        out: &mut Vec<Packet>,
    ) -> anyhow::Result<()> {
        let Self {
            usb,
            inflight,
            state,
            ..
        } = self;
        let State::Active(device) = state else {
            anyhow::bail!("received a usbredir request before peer Hello");
        };
        let RequestPacket { id, kind } = request;

        match kind {
            // Answered from the guest-visible state this bridge owns, never
            // forwarded to the device. Two reasons, and the second one is a
            // bug that was observed rather than reasoned about:
            //
            // - the guest-visible state is the authority the following
            //   SetConfiguration is validated against, so a read must not be
            //   able to change it;
            // - a FreeRDP client serves GET_CONFIGURATION through libusb,
            //   which claims the device's interfaces to do so. The
            //   SELECT_CONFIGURATION that follows then fails with EBUSY out of
            //   USBDEVFS_SETCONFIGURATION, because libusb refuses to change
            //   the configuration while an interface is claimed.
            //
            // `UsbDeviceHandle::get_configuration` and `get_interface` are
            // usable in general; they are just not safe to issue before the
            // configuration has been selected.
            RequestKind::GetConfiguration => out.push(Packet::configuration_status(
                id,
                Status::Success,
                device.configuration(),
            )),
            RequestKind::SetConfiguration { configuration } => {
                set_configuration(usb, device, id, configuration, out).await?;
            }
            RequestKind::GetAltSetting { interface } => match device.alt_setting(interface) {
                Some(alt) => out.push(Packet::alt_setting_status(
                    id,
                    Status::Success,
                    interface,
                    alt,
                )),
                None => out.push(Packet::alt_setting_status(id, Status::Inval, interface, 0)),
            },
            RequestKind::SetAltSetting { interface, alt } => {
                set_alt_setting(usb, device, id, interface, alt, out).await?;
            }
            RequestKind::CancelDataPacket => inflight.cancel(id),

            RequestKind::Reset => reset_device(usb, id).await?,

            RequestKind::StartIsoStream { endpoint, .. }
            | RequestKind::StopIsoStream { endpoint } => {
                out.push(Packet::iso_stream_status(id, Status::Inval, endpoint));
            }
            RequestKind::StartInterruptReceiving { endpoint }
            | RequestKind::StopInterruptReceiving { endpoint } => {
                out.push(Packet::interrupt_receiving_status(
                    id,
                    Status::Inval,
                    endpoint,
                ));
            }
            RequestKind::AllocBulkStreams {
                endpoints,
                no_streams,
            } => {
                out.push(Packet::bulk_streams_status(
                    id,
                    endpoints,
                    no_streams,
                    Status::Inval,
                ));
            }
            RequestKind::FreeBulkStreams { endpoints } => {
                out.push(Packet::bulk_streams_status(id, endpoints, 0, Status::Inval));
            }
            kind => tracing::warn!(id, ?kind, "Ignored unexpected usbredir request from QEMU"),
        }

        Ok(())
    }

    /// Ref: MS-RDPEUSB `UsbDeviceCaps.DeviceSpeed` and usbredir v0.7
    /// `usb_redir_device_connect`.
    ///
    /// RDPEUSB only distinguishes full from high speed, and `bcdUSB` alone does
    /// not prove SuperSpeed, so no other speed can be reported honestly.
    fn device_speed(&self) -> Speed {
        match self.announce.usb_device_caps.device_speed {
            DeviceSpeed::HIGH_SPEED => Speed::High,
            DeviceSpeed::FULL_SPEED => Speed::Full,
            _ => Speed::Unknown,
        }
    }
}

async fn set_configuration(
    usb: &UsbDeviceHandle,
    device: &mut ActiveDevice,
    id: u64,
    configuration: u8,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    // Every failure answers with the configuration QEMU already sees: a failed
    // selection must leave the guest's view untouched.
    let fallback = device.configuration();

    if configuration != 0 {
        // A device that will not hand over its descriptors is reported to the
        // guest rather than ending the session: it is a device fault, not a
        // transport one.
        match device.cache_configuration(usb, configuration).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(id, configuration, "USB configuration does not exist");
                out.push(Packet::configuration_status(id, Status::Inval, fallback));
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(id, configuration, %error, "Failed to load USB configuration descriptor");
                out.push(Packet::configuration_status(id, Status::IoError, fallback));
                return Ok(());
            }
        }
    }

    // A same-value SetConfiguration is still a real selection transaction: it
    // resets endpoints and replaces the RDPEUSB handles the data plane uses.
    let pending = {
        let descriptor = match configuration {
            0 => None,
            _ => {
                let bytes = device
                    .configuration_descriptor(configuration)
                    .context("usb configuration descriptor is not cached")?;
                Some(
                    ConfigurationDescriptorSet::parse(bytes)
                        .context("parse usb configuration descriptor")?,
                )
            }
        };

        usb.select_configuration(descriptor)
            .await
            .context("submit usb configuration selection")?
    };

    let UsbRequestCompletion::Unit(result) = pending.wait().await? else {
        anyhow::bail!("usb configuration selection produced an unexpected completion")
    };
    if let Err(error) = result {
        tracing::warn!(id, configuration, %error, "USB configuration selection failed");
        out.push(Packet::configuration_status(
            id,
            usbredir_status(error),
            fallback,
        ));
        return Ok(());
    }

    device.apply_configuration(configuration)?;

    // usbredir v0.7 requires the metadata to precede the successful status.
    push_metadata(device, out);
    out.push(Packet::configuration_status(
        id,
        Status::Success,
        configuration,
    ));
    Ok(())
}

async fn set_alt_setting(
    usb: &UsbDeviceHandle,
    device: &mut ActiveDevice,
    id: u64,
    interface: u8,
    alt: u8,
    out: &mut Vec<Packet>,
) -> anyhow::Result<()> {
    let Some(fallback) = device.alt_setting(interface) else {
        out.push(Packet::alt_setting_status(id, Status::Inval, interface, 0));
        return Ok(());
    };
    if !device.has_alt_setting(interface, alt) {
        tracing::warn!(id, interface, alt, "USB alternate setting does not exist");
        out.push(Packet::alt_setting_status(
            id,
            Status::Inval,
            interface,
            fallback,
        ));
        return Ok(());
    }

    let pending = {
        let bytes = device
            .active_configuration_descriptor()
            .context("usb device has no active configuration")?;
        let descriptor = ConfigurationDescriptorSet::parse(bytes)
            .context("parse usb configuration descriptor")?;

        usb.select_interface(
            descriptor,
            InterfaceSelection {
                interface,
                alternate_setting: alt,
            },
        )
        .await
        .context("submit usb interface selection")?
    };

    let UsbRequestCompletion::Unit(result) = pending.wait().await? else {
        anyhow::bail!("usb interface selection produced an unexpected completion")
    };
    if let Err(error) = result {
        tracing::warn!(id, interface, alt, %error, "USB interface selection failed");
        out.push(Packet::alt_setting_status(
            id,
            usbredir_status(error),
            interface,
            fallback,
        ));
        return Ok(());
    }

    device.apply_alt_setting(interface, alt)?;

    push_metadata(device, out);
    out.push(Packet::alt_setting_status(
        id,
        Status::Success,
        interface,
        alt,
    ));
    Ok(())
}

/// Resets the redirected device.
///
/// usbredir defines no response for a reset, so a successful one produces no
/// packet at all. The guest-visible view survives unchanged: MS-RDPEUSB resets
/// the port through the client's USB driver stack, which restores the device's
/// configuration and leaves its pipe handles valid, and the facade restores its
/// own binding to match.
///
/// In-flight data requests are deliberately left alone. Their request IDs are
/// already on the wire and the handles they were built from stay valid, so
/// their completions still arrive and still owe QEMU a response.
async fn reset_device(usb: &UsbDeviceHandle, id: u64) -> anyhow::Result<()> {
    let pending = usb
        .reset_device()
        .await
        .context("submit usb device reset")?;

    let UsbRequestCompletion::Unit(result) = pending.wait().await? else {
        anyhow::bail!("usb device reset produced an unexpected completion")
    };

    // A failed reset leaves the binding indeterminate, so every later transfer
    // would fail as well. usbredirhost treats this as device loss; ending the
    // session closes the chardev and lets QEMU tear the device down.
    result.map_err(|error| anyhow::anyhow!("usb device reset failed: {error}"))?;

    tracing::debug!(id, "Reset the redirected USB device");
    Ok(())
}

/// usbredir requires endpoint and interface metadata to precede any change
/// that makes it observable to the guest.
fn push_metadata(device: &ActiveDevice, out: &mut Vec<Packet>) {
    let (ep_info, interface_info) = device.metadata();
    out.push(Packet::ep_info(ep_info));
    out.push(Packet::interface_info(interface_info));
}

pub(super) fn usbredir_status(error: UsbError) -> Status {
    match error {
        UsbError::Cancelled => Status::Cancelled,
        UsbError::Stall => Status::Stall,
        UsbError::Timeout => Status::Timeout,
        UsbError::Overflow => Status::Babble,
        UsbError::NoDevice | UsbError::Error => Status::IoError,
        _ => Status::IoError,
    }
}
