//! The USB device as QEMU sees it, and its usbredir projection.

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context as _};
use ironrdp_server::{UsbDeviceHandle, UsbRequestCompletion};
use ironrdp_usb::{
    control::{GetDescriptorRequest, Recipient},
    descriptor::{
        descriptor_type, ConfigurationDescriptorSet, DeviceDescriptor, EndpointDescriptor,
        InterfaceDescriptor,
    },
    TransferType as UsbTransferType, UsbSpeed,
};
use usbredir_proto::{
    DeviceConnectInfo, EpInfoData, InterfaceInfoData, Speed, TransferType as UsbredirTransferType,
};

/// `bLength` of a standard USB device descriptor.
const DEVICE_DESCRIPTOR_LENGTH: u16 = 18;
/// `bLength` of a configuration descriptor header, which declares `wTotalLength`.
const CONFIGURATION_DESCRIPTOR_HEADER_LENGTH: u16 = 9;

/// Endpoint slots in a usbredir `ep_info` packet: OUT 0-15, then IN 0-15.
const EP_INFO_SLOTS: usize = 32;
const EP_INFO_IN_SLOT_BASE: usize = 16;

pub(super) struct ActiveDevice {
    /// Kept verbatim: usbredir forwards descriptor bytes unchanged, so a quirky
    /// descriptor must survive the round trip byte for byte.
    device_descriptor: Vec<u8>,
    speed: Speed,
    device_connect: DeviceConnectInfo,
    /// Complete configuration descriptor sets, keyed by descriptor index.
    ///
    /// Indexed by position rather than by `bConfigurationValue` because that is
    /// the only key `GET_DESCRIPTOR` accepts; the value is only known after the
    /// descriptor has been read.
    configurations: BTreeMap<u8, Vec<u8>>,
    announced: AnnouncedView,
}

/// The device as QEMU currently sees it.
///
/// Deliberately distinct from the RDPEUSB binding the facade tracks: the two
/// observers diverge whenever a selection fails, because a failed selection
/// must leave the guest's view untouched.
struct AnnouncedView {
    configuration: u8,
    active_interfaces: Vec<(u8, u8)>,
    interface_info: InterfaceInfoData,
    ep_info: EpInfoData,
}

impl ActiveDevice {
    /// Queries the descriptors needed to announce the device to QEMU.
    ///
    /// The guest-visible device starts unconfigured whatever the RDP client
    /// happened to leave it in: QEMU only observes a configuration once its own
    /// SetConfiguration has succeeded.
    pub(super) async fn prepare(usb: &UsbDeviceHandle, speed: Speed) -> anyhow::Result<Self> {
        let device_descriptor =
            get_descriptor(usb, descriptor_type::DEVICE, 0, DEVICE_DESCRIPTOR_LENGTH)
                .await
                .context("query usb device descriptor")?;

        let descriptor =
            DeviceDescriptor::parse(&device_descriptor).context("parse usb device descriptor")?;
        let class = descriptor.class();
        let device_connect = DeviceConnectInfo {
            speed,
            device_class: class.class,
            device_subclass: class.subclass,
            device_protocol: class.protocol,
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
            device_version_bcd: descriptor.device_release().raw(),
        };
        let (interface_info, ep_info) = usbredir_config_info(descriptor, None, &[], speed)?;

        Ok(Self {
            device_descriptor,
            speed,
            device_connect,
            configurations: BTreeMap::new(),
            announced: AnnouncedView {
                configuration: 0,
                active_interfaces: Vec::new(),
                interface_info,
                ep_info,
            },
        })
    }

    pub(super) fn device_connect(&self) -> DeviceConnectInfo {
        self.device_connect.clone()
    }

    /// The endpoint and interface metadata usbredir requires before the device
    /// becomes visible to the guest.
    pub(super) fn metadata(&self) -> (EpInfoData, InterfaceInfoData) {
        (
            self.announced.ep_info.clone(),
            self.announced.interface_info.clone(),
        )
    }

    /// The configuration QEMU currently believes is active.
    pub(super) fn configuration(&self) -> u8 {
        self.announced.configuration
    }

    /// The alternate setting QEMU currently believes is active, if the
    /// interface is part of the active configuration.
    pub(super) fn alt_setting(&self, interface: u8) -> Option<u8> {
        self.announced
            .active_interfaces
            .iter()
            .find_map(|&(number, alt)| (number == interface).then_some(alt))
    }

    /// Cached descriptor bytes of a configuration, by `bConfigurationValue`.
    pub(super) fn configuration_descriptor(&self, configuration: u8) -> Option<&[u8]> {
        self.configurations
            .values()
            .map(Vec::as_slice)
            .find(|bytes| {
                ConfigurationDescriptorSet::parse(bytes)
                    .is_ok_and(|set| set.configuration().configuration_value() == configuration)
            })
    }

    /// Cached descriptor bytes of the configuration QEMU currently sees.
    pub(super) fn active_configuration_descriptor(&self) -> Option<&[u8]> {
        (self.announced.configuration != 0)
            .then(|| self.configuration_descriptor(self.announced.configuration))
            .flatten()
    }

    /// The device descriptor exactly as the device returned it.
    pub(super) fn device_descriptor_bytes(&self) -> &[u8] {
        &self.device_descriptor
    }

    /// Cached descriptor bytes of a configuration, by descriptor index.
    pub(super) fn configuration_descriptor_by_index(&self, index: u8) -> Option<&[u8]> {
        self.configurations.get(&index).map(Vec::as_slice)
    }

    /// Caches a configuration descriptor the guest read for itself.
    ///
    /// Rejecting a malformed descriptor here keeps the cache trustworthy for
    /// the selection path, which builds RDPEUSB requests from it.
    pub(super) fn insert_configuration_descriptor(
        &mut self,
        index: u8,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let set = ConfigurationDescriptorSet::parse(bytes)
            .context("parse usb configuration descriptor")?;
        set.validate()
            .context("invalid usb configuration descriptor")?;

        self.configurations.insert(index, bytes.to_vec());
        Ok(())
    }

    /// Reads configuration descriptors until `configuration` is cached.
    ///
    /// Returns `false` when the device has no such configuration. Descriptors
    /// are read one index at a time because only the header reveals which
    /// `bConfigurationValue` an index carries.
    pub(super) async fn cache_configuration(
        &mut self,
        usb: &UsbDeviceHandle,
        configuration: u8,
    ) -> anyhow::Result<bool> {
        if self.configuration_descriptor(configuration).is_some() {
            return Ok(true);
        }

        let device = DeviceDescriptor::parse(&self.device_descriptor)
            .context("parse usb device descriptor")?;
        for index in 0..device.num_configurations() {
            if self.configurations.contains_key(&index) {
                continue;
            }

            let header = get_descriptor(
                usb,
                descriptor_type::CONFIGURATION,
                index,
                CONFIGURATION_DESCRIPTOR_HEADER_LENGTH,
            )
            .await
            .with_context(|| format!("query usb configuration descriptor {index} header"))?;
            let total_length = ConfigurationDescriptorSet::required_length(&header)
                .context("parse usb configuration descriptor header")?
                .context("usb configuration descriptor header is incomplete")?;
            let total_length =
                u16::try_from(total_length).context("usb configuration descriptor is too large")?;

            let bytes = get_descriptor(usb, descriptor_type::CONFIGURATION, index, total_length)
                .await
                .with_context(|| format!("query usb configuration descriptor {index}"))?;
            let set = ConfigurationDescriptorSet::parse(&bytes)
                .with_context(|| format!("parse usb configuration descriptor {index}"))?;
            set.validate()
                .with_context(|| format!("invalid usb configuration descriptor {index}"))?;
            let value = set.configuration().configuration_value();

            self.configurations.insert(index, bytes);
            if value == configuration {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub(super) fn has_alt_setting(&self, interface: u8, alt: u8) -> bool {
        let Some(bytes) = self.active_configuration_descriptor() else {
            return false;
        };
        ConfigurationDescriptorSet::parse(bytes)
            .is_ok_and(|set| set.interface(interface, alt).is_some())
    }

    /// Makes `configuration` the configuration QEMU sees, with alternate
    /// setting zero on every interface.
    ///
    /// Mirrors [`UsbDeviceHandle::select_configuration`], which always selects
    /// the default alternate settings. Only called once the selection has
    /// succeeded, so a failure leaves the previous view in place.
    pub(super) fn apply_configuration(&mut self, configuration: u8) -> anyhow::Result<()> {
        let device = DeviceDescriptor::parse(&self.device_descriptor)
            .context("parse usb device descriptor")?;

        if configuration == 0 {
            let (interface_info, ep_info) = usbredir_config_info(device, None, &[], self.speed)?;
            self.announced = AnnouncedView {
                configuration: 0,
                active_interfaces: Vec::new(),
                interface_info,
                ep_info,
            };
            return Ok(());
        }

        let bytes = self
            .configuration_descriptor(configuration)
            .with_context(|| format!("usb configuration {configuration} is not cached"))?;
        let set = ConfigurationDescriptorSet::parse(bytes)
            .context("parse usb configuration descriptor")?;
        let active_interfaces: Vec<(u8, u8)> = set
            .default_interfaces()
            .map(|interface| (interface.number(), interface.alternate_setting()))
            .collect();
        let (interface_info, ep_info) =
            usbredir_config_info(device, Some(set), &active_interfaces, self.speed)?;

        self.announced = AnnouncedView {
            configuration,
            active_interfaces,
            interface_info,
            ep_info,
        };
        Ok(())
    }

    /// Makes `alt` the alternate setting QEMU sees for `interface`.
    ///
    /// Only called once the selection has succeeded.
    pub(super) fn apply_alt_setting(&mut self, interface: u8, alt: u8) -> anyhow::Result<()> {
        let device = DeviceDescriptor::parse(&self.device_descriptor)
            .context("parse usb device descriptor")?;
        let bytes = self
            .active_configuration_descriptor()
            .context("usb device has no active configuration")?;
        let set = ConfigurationDescriptorSet::parse(bytes)
            .context("parse usb configuration descriptor")?;

        let mut active_interfaces = self.announced.active_interfaces.clone();
        let selected = active_interfaces
            .iter_mut()
            .find_map(|(number, selected)| (*number == interface).then_some(selected))
            .with_context(|| format!("usb interface {interface} is not active"))?;
        *selected = alt;

        let (interface_info, ep_info) =
            usbredir_config_info(device, Some(set), &active_interfaces, self.speed)?;

        self.announced.active_interfaces = active_interfaces;
        self.announced.interface_info = interface_info;
        self.announced.ep_info = ep_info;
        Ok(())
    }
}

/// Projects a USB configuration onto the usbredir interface and endpoint
/// metadata packets.
///
/// Ref: usbredir protocol v0.7, `usb_redir_interface_info` and
/// `usb_redir_ep_info`. Only active alternate settings are reported; endpoint
/// slots are OUT 0-15 followed by IN 0-15.
fn usbredir_config_info(
    device: DeviceDescriptor<'_>,
    config: Option<ConfigurationDescriptorSet<'_>>,
    active_interfaces: &[(u8, u8)],
    speed: Speed,
) -> anyhow::Result<(InterfaceInfoData, EpInfoData)> {
    ensure!(
        active_interfaces.len() <= EP_INFO_SLOTS,
        "usbredir supports at most {EP_INFO_SLOTS} active usb interfaces"
    );

    let mut interface_info = InterfaceInfoData {
        interface_count: active_interfaces.len() as u32,
        interface: [0; EP_INFO_SLOTS],
        interface_class: [0; EP_INFO_SLOTS],
        interface_subclass: [0; EP_INFO_SLOTS],
        interface_protocol: [0; EP_INFO_SLOTS],
    };
    let mut ep_info = EpInfoData {
        ep_type: [UsbredirTransferType::Invalid; EP_INFO_SLOTS],
        interval: [0; EP_INFO_SLOTS],
        interface: [0; EP_INFO_SLOTS],
        max_packet_size: [0; EP_INFO_SLOTS],
        max_streams: [0; EP_INFO_SLOTS],
    };

    // The default control pipe occupies both direction slots of endpoint zero
    // and exists whether or not the device is configured.
    let usb_speed = usb_speed(speed)?;
    let max_packet_size_0 = device
        .endpoint_zero_max_packet_size(usb_speed)
        .context("interpret usb bMaxPacketSize0")?;
    for slot in [0, EP_INFO_IN_SLOT_BASE] {
        ep_info.ep_type[slot] = UsbredirTransferType::Control;
        ep_info.max_packet_size[slot] = max_packet_size_0;
    }

    let Some(config) = config else {
        return Ok((interface_info, ep_info));
    };

    for (position, &(number, alternate_setting)) in active_interfaces.iter().enumerate() {
        let interface = config
            .interface(number, alternate_setting)
            .with_context(|| {
                format!("usb interface {number} has no alternate setting {alternate_setting}")
            })?;
        let class = interface.class();
        interface_info.interface[position] = number;
        interface_info.interface_class[position] = class.class;
        interface_info.interface_subclass[position] = class.subclass;
        interface_info.interface_protocol[position] = class.protocol;

        for endpoint in interface.endpoints() {
            let address = endpoint.address().with_context(|| {
                format!(
                    "invalid usb endpoint address {:#04x}",
                    endpoint.address_raw()
                )
            })?;
            let slot = endpoint_slot(address.raw());
            ensure!(
                ep_info.ep_type[slot] == UsbredirTransferType::Invalid,
                "duplicate active usb endpoint address {:#04x}",
                address.raw()
            );

            ep_info.ep_type[slot] = usbredir_transfer_type(endpoint.transfer_type());
            ep_info.interval[slot] = usbredir_interval(endpoint, speed);
            ep_info.interface[slot] = number;
            let max_packet_size = endpoint.max_packet_size();
            ep_info.max_packet_size[slot] = max_packet_size
                .usb2_max_payload(usb_speed, endpoint.transfer_type())
                .with_context(|| {
                    format!(
                        "usb endpoint {:#04x} has invalid wMaxPacketSize {:#06x}",
                        address.raw(),
                        max_packet_size.raw()
                    )
                })?;
            ep_info.max_streams[slot] = endpoint_max_streams(interface, endpoint);
        }
    }

    Ok((interface_info, ep_info))
}

fn endpoint_slot(address: u8) -> usize {
    usize::from(address & 0x0f)
        + if address & 0x80 == 0 {
            0
        } else {
            EP_INFO_IN_SLOT_BASE
        }
}

fn usbredir_transfer_type(transfer_type: UsbTransferType) -> UsbredirTransferType {
    match transfer_type {
        UsbTransferType::Control => UsbredirTransferType::Control,
        UsbTransferType::Isochronous => UsbredirTransferType::Iso,
        UsbTransferType::Bulk => UsbredirTransferType::Bulk,
        UsbTransferType::Interrupt => UsbredirTransferType::Interrupt,
    }
}

/// usbredir reports the polling interval in frames or microframes for periodic
/// endpoints, so the exponential high-speed encoding has to be expanded.
fn usbredir_interval(endpoint: EndpointDescriptor<'_>, speed: Speed) -> u8 {
    if !matches!(
        endpoint.transfer_type(),
        UsbTransferType::Isochronous | UsbTransferType::Interrupt
    ) {
        return 0;
    }
    if !matches!(speed, Speed::High | Speed::Super) || endpoint.interval() == 0 {
        return endpoint.interval();
    }

    let shift = u32::from(endpoint.interval().saturating_sub(1)).min(15);
    (1u32 << shift).min(u32::from(u8::MAX)) as u8
}

/// Bulk stream count from the SuperSpeed endpoint companion descriptor that
/// follows the endpoint, if any.
fn endpoint_max_streams(
    interface: InterfaceDescriptor<'_>,
    endpoint: EndpointDescriptor<'_>,
) -> u32 {
    if endpoint.transfer_type() != UsbTransferType::Bulk {
        return 0;
    }

    let mut belongs_to_endpoint = false;
    for descriptor in interface.descriptors() {
        match descriptor.descriptor_type() {
            descriptor_type::ENDPOINT => {
                belongs_to_endpoint = descriptor.offset() == endpoint.offset();
            }
            descriptor_type::SUPER_SPEED_ENDPOINT_COMPANION if belongs_to_endpoint => {
                let bytes = descriptor.as_bytes();
                if bytes.len() >= 6 {
                    let exponent = bytes[3] & 0x1f;
                    return if exponent == 0 { 0 } else { 1u32 << exponent };
                }
            }
            _ => {}
        }
    }

    0
}

fn usb_speed(speed: Speed) -> anyhow::Result<UsbSpeed> {
    match speed {
        Speed::Low => Ok(UsbSpeed::Low),
        Speed::Full => Ok(UsbSpeed::Full),
        Speed::High => Ok(UsbSpeed::High),
        Speed::Super => Ok(UsbSpeed::Super),
        // Endpoint-zero packet size cannot be interpreted without a speed, so
        // the device cannot be announced at all.
        Speed::Unknown => bail!("usb device speed is unknown"),
        speed => bail!("unsupported usb device speed {speed:?}"),
    }
}

async fn get_descriptor(
    usb: &UsbDeviceHandle,
    descriptor_type: u8,
    descriptor_index: u8,
    requested_length: u16,
) -> anyhow::Result<Vec<u8>> {
    let pending = usb
        .get_descriptor(GetDescriptorRequest {
            recipient: Recipient::DEVICE,
            descriptor_type,
            descriptor_index,
            index: 0,
            requested_length,
        })
        .await?;

    let UsbRequestCompletion::Descriptor(result) = pending.wait().await? else {
        bail!("usb get descriptor produced an unexpected completion")
    };

    result.map_err(|error| anyhow::anyhow!("usb get descriptor failed: {error}"))
}
