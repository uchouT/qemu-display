//! USB descriptor cache adapters and usbredir metadata projection.

use std::collections::BTreeMap;

use anyhow::{bail, ensure};
use ironrdp_rdpeusb::{
    pdu::completion::ts_urb_result::{TsUsbdInterfaceInfoResult, UsbdPipeType},
    usb as rdpeusb,
};
use ironrdp_usb::{
    descriptor::{
        descriptor_type, ConfigurationDescriptorSet, DeviceDescriptor as UsbDeviceDescriptor,
        EndpointDescriptor, InterfaceDescriptor,
    },
    value::{TransferType as UsbTransferType, UsbSpeed},
};
use usbredir_proto::{EpInfoData, InterfaceInfoData, Speed, TransferType as UsbredirTransferType};

pub(crate) const DEVICE_DESCRIPTOR_SIZE: usize = 18;
pub(crate) const CONFIG_DESCRIPTOR_SIZE: usize = 9;

/// Owned device identity used by the usbredir state model.
///
/// `ironrdp-usb` intentionally exposes borrowed descriptor views. This small
/// owned projection retains only the fields needed after enumeration; all USB
/// parsing and validation remains in `ironrdp-usb`.
#[derive(Debug, Clone)]
pub(crate) struct DeviceDescriptor {
    bytes: Vec<u8>,
    pub(crate) device_class: u8,
    pub(crate) device_subclass: u8,
    pub(crate) device_protocol: u8,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) device_version: u16,
    num_configurations: u8,
}

impl DeviceDescriptor {
    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let descriptor = UsbDeviceDescriptor::parse(bytes)
            .map_err(|error| anyhow::anyhow!("invalid USB device descriptor: {error}"))?;
        descriptor
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid USB device descriptor: {error}"))?;
        let class = descriptor.class();

        Ok(Self {
            bytes: descriptor.as_bytes().to_vec(),
            device_class: class.class,
            device_subclass: class.subclass,
            device_protocol: class.protocol,
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
            device_version: descriptor.device_release(),
            num_configurations: descriptor.num_configurations(),
        })
    }

    fn view(&self) -> UsbDeviceDescriptor<'_> {
        UsbDeviceDescriptor::parse(&self.bytes)
            .expect("cached USB device descriptor was validated when inserted")
    }

    fn max_packet_size_0(&self, speed: UsbSpeed) -> anyhow::Result<u16> {
        self.view()
            .endpoint_zero_max_packet_size(speed)
            .map_err(|error| anyhow::anyhow!("invalid USB endpoint zero packet size: {error}"))
    }

    pub(crate) fn num_configurations(&self) -> u8 {
        self.num_configurations
    }
}

/// An owned, validated USB configuration descriptor set.
///
/// The public USB model is borrowed and allocation-free. qemu-rdp owns the
/// bytes because descriptors must survive across usbredir requests, and
/// reparses a borrowed view only when it needs to query the topology.
#[derive(Debug, Clone)]
pub(crate) struct ConfigurationDescriptor {
    bytes: Vec<u8>,
}

impl ConfigurationDescriptor {
    pub(crate) fn parse_header(bytes: &[u8]) -> anyhow::Result<(usize, u8)> {
        ensure!(
            bytes.len() >= CONFIG_DESCRIPTOR_SIZE,
            "USB configuration descriptor header is truncated"
        );
        let total_length = ConfigurationDescriptorSet::required_length(bytes)
            .map_err(|error| anyhow::anyhow!("invalid USB configuration descriptor: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("USB configuration descriptor header is truncated"))?;
        Ok((total_length, bytes[5]))
    }

    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let descriptor = ConfigurationDescriptorSet::parse(bytes)
            .map_err(|error| anyhow::anyhow!("invalid USB configuration descriptor: {error}"))?;
        descriptor
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid USB configuration descriptor: {error}"))?;
        Ok(Self {
            bytes: descriptor.as_bytes().to_vec(),
        })
    }

    /// Parses a configuration descriptor only when the response contains the
    /// complete `wTotalLength`. Enumeration commonly asks for a short prefix
    /// first; that prefix is useful to the guest but is not cacheable.
    pub(crate) fn parse_if_complete(bytes: &[u8]) -> anyhow::Result<Option<Self>> {
        let Some(descriptor) = ConfigurationDescriptorSet::parse_if_complete(bytes)
            .map_err(|error| anyhow::anyhow!("invalid USB configuration descriptor: {error}"))?
        else {
            return Ok(None);
        };
        descriptor
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid USB configuration descriptor: {error}"))?;
        Ok(Some(Self {
            bytes: descriptor.as_bytes().to_vec(),
        }))
    }

    pub(crate) fn view(&self) -> ConfigurationDescriptorSet<'_> {
        ConfigurationDescriptorSet::parse(&self.bytes)
            .expect("cached USB configuration descriptor was validated when inserted")
    }

    pub(crate) fn configuration_value(&self) -> u8 {
        self.view().configuration().configuration_value()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn default_alt_settings(&self) -> anyhow::Result<Vec<(u8, u8)>> {
        let mut selections = self
            .view()
            .default_interfaces()
            .map(|interface| (interface.number(), interface.alternate_setting()))
            .collect::<Vec<_>>();
        // Preserve the old canonical order used in usbredir announcements and
        // make request order independent of descriptor layout.
        selections.sort_unstable_by_key(|&(interface, _)| interface);
        Ok(selections)
    }

    pub(crate) fn contains_alt_setting(&self, interface: u8, alt: u8) -> bool {
        self.view().interface(interface, alt).is_some()
    }

    pub(crate) fn validate_interface_result(
        &self,
        interface: u8,
        alt: u8,
        speed: UsbSpeed,
        result: &TsUsbdInterfaceInfoResult,
    ) -> anyhow::Result<()> {
        let expected = self.interface(interface, alt)?;
        ensure!(
            result.interface_number == interface,
            "RDPEUSB returned interface {} for requested interface {interface}",
            result.interface_number
        );
        ensure!(
            result.alternate_setting == alt,
            "RDPEUSB returned alternate setting {} for requested interface {interface} alt {alt}",
            result.alternate_setting
        );
        let class = expected.class();
        ensure!(
            result.class == class.class
                && result.sub_class == class.subclass
                && result.protocol == class.protocol,
            "RDPEUSB returned descriptor identity inconsistent with interface {interface} alt {alt}"
        );

        let expected_endpoints = expected
            .endpoints()
            .map(|endpoint| (endpoint.address_raw(), endpoint))
            .collect::<BTreeMap<_, _>>();
        ensure!(
            result.pipes.len() == expected_endpoints.len(),
            "RDPEUSB returned {} pipes for interface {interface} alt {alt}, expected {}",
            result.pipes.len(),
            expected_endpoints.len()
        );

        let mut returned_endpoints = BTreeMap::new();
        for pipe in &result.pipes {
            let endpoint = expected_endpoints
                .get(&pipe.endpoint_address)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "RDPEUSB returned unexpected endpoint {:#04x} for interface {interface} alt {alt}",
                        pipe.endpoint_address
                    )
                })?;
            ensure!(
                returned_endpoints
                    .insert(pipe.endpoint_address, ())
                    .is_none(),
                "RDPEUSB returned duplicate endpoint {:#04x} for interface {interface} alt {alt}",
                pipe.endpoint_address
            );
            ensure!(
                pipe.pipe_type == usbd_pipe_type(endpoint.transfer_type()),
                "RDPEUSB returned the wrong pipe type for endpoint {:#04x}",
                pipe.endpoint_address
            );
            let max_packet_size = rdpeusb::maximum_packet_size(
                endpoint.max_packet_size(),
                endpoint.transfer_type(),
                speed,
            )
            .map_err(|raw| {
                anyhow::anyhow!(
                    "USB endpoint {:#04x} has invalid wMaxPacketSize {raw:#06x}",
                    endpoint.address_raw()
                )
            })?;
            ensure!(
                pipe.max_packet_size == max_packet_size,
                "RDPEUSB returned max packet size {} for endpoint {:#04x}, expected {}",
                pipe.max_packet_size,
                pipe.endpoint_address,
                max_packet_size
            );
            // Do not compare Interval here: USBD_PIPE_INFORMATION can expose
            // a host-stack-normalized polling interval, while usbredir EpInfo
            // is derived from the USB descriptor's bInterval semantics.
        }

        Ok(())
    }

    fn interface(&self, interface: u8, alt: u8) -> anyhow::Result<InterfaceDescriptor<'_>> {
        self.view().interface(interface, alt).ok_or_else(|| {
            anyhow::anyhow!("USB interface {interface} has no alternate setting {alt}")
        })
    }
}

pub(crate) fn usb_speed(speed: Speed) -> anyhow::Result<UsbSpeed> {
    match speed {
        Speed::Low => Ok(UsbSpeed::Low),
        Speed::Full => Ok(UsbSpeed::Full),
        Speed::High => Ok(UsbSpeed::High),
        Speed::Super => Ok(UsbSpeed::Super),
        Speed::Unknown => bail!("USB device speed is unknown"),
        _ => bail!("unsupported usbredir USB speed {speed}"),
    }
}

pub(crate) fn usbredir_config_info(
    device: &DeviceDescriptor,
    config: Option<&ConfigurationDescriptor>,
    active_interfaces: &[(u8, u8)],
    speed: Speed,
) -> anyhow::Result<(InterfaceInfoData, EpInfoData)> {
    // Ref: usbredir protocol v0.7, `usb_redir_interface_info` and
    // `usb_redir_ep_info`. Report active alternate settings; endpoint slots
    // are OUT 0-15 followed by IN 0-15.
    ensure!(
        active_interfaces.len() <= 32,
        "usbredir supports at most 32 active USB interfaces"
    );

    let mut interface_info = InterfaceInfoData {
        interface_count: active_interfaces.len() as u32,
        interface: [0; 32],
        interface_class: [0; 32],
        interface_subclass: [0; 32],
        interface_protocol: [0; 32],
    };
    let mut ep_info = EpInfoData {
        ep_type: [UsbredirTransferType::Invalid; 32],
        interval: [0; 32],
        interface: [0; 32],
        max_packet_size: [0; 32],
        max_streams: [0; 32],
    };

    let usb_speed = usb_speed(speed)?;
    let max_packet_size_0 = device.max_packet_size_0(usb_speed)?;
    for index in [0, 16] {
        ep_info.ep_type[index] = UsbredirTransferType::Control;
        ep_info.max_packet_size[index] = max_packet_size_0;
    }

    let Some(config) = config else {
        return Ok((interface_info, ep_info));
    };
    let descriptor = config.view();

    for (position, &(number, alt_setting)) in active_interfaces.iter().enumerate() {
        let interface = descriptor.interface(number, alt_setting).ok_or_else(|| {
            anyhow::anyhow!("USB interface {number} has no alternate setting {alt_setting}")
        })?;
        let class = interface.class();
        interface_info.interface[position] = number;
        interface_info.interface_class[position] = class.class;
        interface_info.interface_subclass[position] = class.subclass;
        interface_info.interface_protocol[position] = class.protocol;

        for endpoint in interface.endpoints() {
            let address = endpoint.address().map_err(|error| {
                anyhow::anyhow!(
                    "invalid USB endpoint address {:#04x}: {error}",
                    endpoint.address_raw()
                )
            })?;
            let index = endpoint_index(address.raw());
            ensure!(
                ep_info.ep_type[index] == UsbredirTransferType::Invalid,
                "duplicate active USB endpoint address {:#04x}",
                address.raw()
            );
            ep_info.ep_type[index] = usbredir_transfer_type(endpoint.transfer_type());
            ep_info.interval[index] = usbredir_interval(endpoint, speed);
            ep_info.interface[index] = number;
            ep_info.max_packet_size[index] = rdpeusb::maximum_packet_size(
                endpoint.max_packet_size(),
                endpoint.transfer_type(),
                usb_speed,
            )
            .map_err(|raw| {
                anyhow::anyhow!(
                    "USB endpoint {:#04x} has invalid wMaxPacketSize {raw:#06x}",
                    address.raw()
                )
            })?;
            ep_info.max_streams[index] = endpoint_max_streams(interface, endpoint);
        }
    }

    Ok((interface_info, ep_info))
}

fn endpoint_index(address: u8) -> usize {
    usize::from(address & 0x0f) + if address & 0x80 == 0 { 0 } else { 16 }
}

fn usbredir_transfer_type(transfer_type: UsbTransferType) -> UsbredirTransferType {
    match transfer_type {
        UsbTransferType::Control => UsbredirTransferType::Control,
        UsbTransferType::Isochronous => UsbredirTransferType::Iso,
        UsbTransferType::Bulk => UsbredirTransferType::Bulk,
        UsbTransferType::Interrupt => UsbredirTransferType::Interrupt,
    }
}

fn usbd_pipe_type(transfer_type: UsbTransferType) -> UsbdPipeType {
    match transfer_type {
        UsbTransferType::Control => UsbdPipeType::Control,
        UsbTransferType::Isochronous => UsbdPipeType::Isochronous,
        UsbTransferType::Bulk => UsbdPipeType::Bulk,
        UsbTransferType::Interrupt => UsbdPipeType::Interrupt,
    }
}

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

#[cfg(test)]
mod tests {
    use ironrdp_rdpeusb::pdu::completion::ts_urb_result::{
        TsUsbdInterfaceInfoResult, TsUsbdPipeInfoResult, UsbdPipeType,
    };
    use ironrdp_usb::value::UsbSpeed;

    use super::ConfigurationDescriptor;

    const CONFIGURATION: [u8; 48] = [
        9, 2, 48, 0, 1, 2, 7, 0x80, 50, // configuration 2
        9, 4, 3, 0, 2, 0xff, 1, 2, 0, // interface 3, alt 0
        7, 5, 0x01, 2, 64, 0, 0, // bulk OUT endpoint 1
        7, 5, 0x81, 3, 8, 0, 10, // interrupt IN endpoint 1
        9, 4, 3, 1, 1, 0xfe, 3, 4, 0, // interface 3, alt 1
        7, 5, 0x82, 1, 128, 0, 1, // isochronous IN endpoint 2
    ];

    #[test]
    fn delegates_parsing_and_topology_queries_to_ironrdp_usb() {
        let descriptor = ConfigurationDescriptor::parse(&CONFIGURATION).unwrap();

        assert_eq!(descriptor.configuration_value(), 2);
        assert_eq!(descriptor.default_alt_settings().unwrap(), vec![(3, 0)]);
        assert!(descriptor.contains_alt_setting(3, 1));
        assert!(!descriptor.contains_alt_setting(3, 2));
    }

    #[test]
    fn validates_selection_results_by_full_endpoint_address() {
        let descriptor = ConfigurationDescriptor::parse(&CONFIGURATION).unwrap();
        let result = TsUsbdInterfaceInfoResult {
            interface_number: 3,
            alternate_setting: 0,
            class: 0xff,
            sub_class: 1,
            protocol: 2,
            interface_handle: 0x1111_2222,
            // Deliberately reverse descriptor order. Result association is by
            // full address, so OUT 1 and IN 1 cannot collide.
            pipes: vec![
                TsUsbdPipeInfoResult {
                    max_packet_size: 8,
                    endpoint_address: 0x81,
                    interval: 10,
                    pipe_type: UsbdPipeType::Interrupt,
                    pipe_handle: 0xaaaa_0001,
                    max_transfer_size: 4096,
                    pipe_flags: 0,
                },
                TsUsbdPipeInfoResult {
                    max_packet_size: 64,
                    endpoint_address: 0x01,
                    interval: 0,
                    pipe_type: UsbdPipeType::Bulk,
                    pipe_handle: 0xbbbb_0001,
                    max_transfer_size: 65_536,
                    pipe_flags: 0,
                },
            ],
        };

        descriptor
            .validate_interface_result(3, 0, UsbSpeed::Full, &result)
            .unwrap();

        let mut duplicate = result;
        duplicate.pipes[1].endpoint_address = 0x81;
        assert!(descriptor
            .validate_interface_result(3, 0, UsbSpeed::Full, &duplicate)
            .is_err());
    }
}
