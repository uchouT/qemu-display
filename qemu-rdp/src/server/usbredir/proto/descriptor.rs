//! USB descriptor parsing and conversion into usbredir protocol metadata.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, ensure};
use ironrdp_rdpeusb::pdu::{
    completion::ts_urb_result::{TsUsbdInterfaceInfoResult, UsbdPipeType},
    usb_dev::ts_urb::utils::{TsUsbdInterfaceInfo, TsUsbdPipeInfo, UsbConfigDesc},
};
use usbredir_proto::{EpInfoData, InterfaceInfoData, Speed, TransferType};

pub(crate) const DEVICE_DESCRIPTOR_TYPE: u8 = 0x01;
pub(crate) const CONFIG_DESCRIPTOR_TYPE: u8 = 0x02;
const INTERFACE_DESCRIPTOR_TYPE: u8 = 0x04;
const ENDPOINT_DESCRIPTOR_TYPE: u8 = 0x05;
const SS_ENDPOINT_COMPANION_DESCRIPTOR_TYPE: u8 = 0x30;

pub(crate) const DEVICE_DESCRIPTOR_SIZE: usize = 18;
pub(crate) const CONFIG_DESCRIPTOR_SIZE: usize = 9;

#[derive(Debug, Clone)]
pub(crate) struct DeviceDescriptor {
    pub(crate) usb_version: u16,
    pub(crate) device_class: u8,
    pub(crate) device_subclass: u8,
    pub(crate) device_protocol: u8,
    pub(crate) max_packet_size_0: u8,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) device_version: u16,
    pub(crate) num_configurations: u8,
}

impl DeviceDescriptor {
    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() >= DEVICE_DESCRIPTOR_SIZE,
            "USB device descriptor is truncated"
        );
        ensure!(
            usize::from(bytes[0]) >= DEVICE_DESCRIPTOR_SIZE,
            "invalid USB device descriptor length"
        );
        ensure!(
            bytes[1] == DEVICE_DESCRIPTOR_TYPE,
            "unexpected USB device descriptor type {:#04x}",
            bytes[1]
        );

        Ok(Self {
            usb_version: u16::from_le_bytes([bytes[2], bytes[3]]),
            device_class: bytes[4],
            device_subclass: bytes[5],
            device_protocol: bytes[6],
            max_packet_size_0: bytes[7],
            vendor_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
            device_version: u16::from_le_bytes([bytes[12], bytes[13]]),
            num_configurations: bytes[17],
        })
    }

    fn max_packet_size_0(&self) -> anyhow::Result<u16> {
        if self.usb_version >= 0x0300 {
            ensure!(
                self.max_packet_size_0 < 16,
                "invalid SuperSpeed endpoint zero packet-size exponent"
            );
            Ok(1u16 << self.max_packet_size_0)
        } else {
            ensure!(
                self.max_packet_size_0 != 0,
                "endpoint zero has a zero max packet size"
            );
            Ok(u16::from(self.max_packet_size_0))
        }
    }

    pub(crate) fn num_configurations(&self) -> u8 {
        self.num_configurations
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigurationDescriptor {
    bytes: Vec<u8>,
    length: u8,
    descriptor_type: u8,
    total_length: u16,
    num_interfaces: u8,
    configuration_value: u8,
    configuration: u8,
    attributes: u8,
    max_power: u8,
    pub(crate) interfaces: BTreeMap<u8, BTreeMap<u8, InterfaceDescriptor>>,
}

impl ConfigurationDescriptor {
    pub(crate) fn parse_header(bytes: &[u8]) -> anyhow::Result<(usize, u8)> {
        ensure!(
            bytes.len() >= CONFIG_DESCRIPTOR_SIZE,
            "USB configuration descriptor header is truncated"
        );
        ensure!(
            usize::from(bytes[0]) == CONFIG_DESCRIPTOR_SIZE,
            "invalid USB configuration descriptor length"
        );
        ensure!(
            bytes[1] == CONFIG_DESCRIPTOR_TYPE,
            "unexpected USB configuration descriptor type {:#04x}",
            bytes[1]
        );

        let total_length = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
        ensure!(
            total_length >= CONFIG_DESCRIPTOR_SIZE,
            "invalid USB configuration total length {total_length}"
        );
        Ok((total_length, bytes[5]))
    }

    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let (total_length, configuration_value) = Self::parse_header(bytes)?;
        ensure!(
            bytes.len() >= total_length,
            "USB configuration descriptor is truncated"
        );

        let mut interfaces: BTreeMap<u8, BTreeMap<u8, InterfaceDescriptor>> = BTreeMap::new();
        let mut current_interface = None;
        let mut current_endpoint = None;
        let mut offset = 0;

        while offset < total_length {
            ensure!(
                offset + 2 <= total_length,
                "truncated USB descriptor header at offset {offset}"
            );
            let length = usize::from(bytes[offset]);
            ensure!(length >= 2, "zero-length USB descriptor at offset {offset}");
            ensure!(
                offset + length <= total_length,
                "USB descriptor at offset {offset} exceeds configuration length"
            );

            match bytes[offset + 1] {
                INTERFACE_DESCRIPTOR_TYPE if length >= 9 => {
                    let interface = InterfaceDescriptor {
                        number: bytes[offset + 2],
                        alternate_setting: bytes[offset + 3],
                        num_endpoints: bytes[offset + 4],
                        class: bytes[offset + 5],
                        subclass: bytes[offset + 6],
                        protocol: bytes[offset + 7],
                        endpoints: Vec::new(),
                    };
                    let key = (interface.number, interface.alternate_setting);
                    let previous = interfaces
                        .entry(key.0)
                        .or_default()
                        .insert(key.1, interface);
                    ensure!(
                        previous.is_none(),
                        "duplicate USB interface {} alternate setting {}",
                        key.0,
                        key.1
                    );
                    current_interface = Some(key);
                    current_endpoint = None;
                }
                ENDPOINT_DESCRIPTOR_TYPE if length >= 7 => {
                    let Some((number, alt)) = current_interface else {
                        bail!("USB endpoint descriptor appears before an interface descriptor");
                    };
                    let address = bytes[offset + 2];
                    ensure!(
                        address & 0x70 == 0,
                        "USB endpoint descriptor has invalid address {address:#04x}"
                    );
                    ensure!(
                        address & 0x0f != 0,
                        "USB interface descriptor contains endpoint zero"
                    );
                    let endpoint = EndpointDescriptor {
                        address,
                        transfer_type: transfer_type(bytes[offset + 3] & 0x03),
                        interval: bytes[offset + 6],
                        max_packet_size: actual_max_packet_size(u16::from_le_bytes([
                            bytes[offset + 4],
                            bytes[offset + 5],
                        ])),
                        max_streams: 0,
                    };
                    let endpoints = &mut interfaces
                        .get_mut(&number)
                        .and_then(|alternates| alternates.get_mut(&alt))
                        .expect("current interface was inserted")
                        .endpoints;
                    endpoints.push(endpoint);
                    current_endpoint = Some((number, alt, endpoints.len() - 1));
                }
                SS_ENDPOINT_COMPANION_DESCRIPTOR_TYPE if length >= 6 => {
                    if let Some((number, alt, endpoint)) = current_endpoint {
                        let endpoint = &mut interfaces
                            .get_mut(&number)
                            .and_then(|alternates| alternates.get_mut(&alt))
                            .expect("current interface was inserted")
                            .endpoints[endpoint];
                        if endpoint.transfer_type == TransferType::Bulk {
                            let exponent = bytes[offset + 3] & 0x1f;
                            endpoint.max_streams = if exponent == 0 { 0 } else { 1u32 << exponent };
                        }
                    }
                }
                _ => {}
            }

            offset += length;
        }

        ensure!(
            interfaces.len() == usize::from(bytes[4]),
            "USB configuration declares {} interfaces but contains {}",
            bytes[4],
            interfaces.len()
        );
        for (&number, alternates) in &interfaces {
            for (&alt, interface) in alternates {
                ensure!(
                    interface.endpoints.len() == usize::from(interface.num_endpoints),
                    "USB interface {number} alt {alt} declares {} endpoints but contains {}",
                    interface.num_endpoints,
                    interface.endpoints.len()
                );
                let mut endpoints = BTreeSet::new();
                for endpoint in &interface.endpoints {
                    ensure!(
                        endpoints.insert(endpoint.address),
                        "USB interface {number} alt {alt} contains duplicate endpoint {:#04x}",
                        endpoint.address
                    );
                }
            }
        }

        Ok(Self {
            bytes: bytes[..total_length].to_vec(),
            length: bytes[0],
            descriptor_type: bytes[1],
            total_length: u16::from_le_bytes([bytes[2], bytes[3]]),
            num_interfaces: bytes[4],
            configuration_value,
            configuration: bytes[6],
            attributes: bytes[7],
            max_power: bytes[8],
            interfaces,
        })
    }

    /// Parses a configuration descriptor only when the response contains the
    /// complete `wTotalLength`. Enumeration commonly asks for a short prefix
    /// first; that prefix is useful to the guest but is not cacheable.
    pub(crate) fn parse_if_complete(bytes: &[u8]) -> anyhow::Result<Option<Self>> {
        if bytes.len() < CONFIG_DESCRIPTOR_SIZE {
            return Ok(None);
        }

        let (total_length, _) = Self::parse_header(bytes)?;
        if bytes.len() < total_length {
            return Ok(None);
        }

        Self::parse(&bytes[..total_length]).map(Some)
    }

    pub(crate) fn configuration_value(&self) -> u8 {
        self.configuration_value
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn default_alt_settings(&self) -> anyhow::Result<Vec<(u8, u8)>> {
        self.interfaces
            .iter()
            .map(|(&interface, alternates)| {
                ensure!(
                    alternates.contains_key(&0),
                    "USB interface {interface} has no default alternate setting 0"
                );
                Ok((interface, 0))
            })
            .collect()
    }

    pub(crate) fn contains_alt_setting(&self, interface: u8, alt: u8) -> bool {
        self.interfaces
            .get(&interface)
            .is_some_and(|alternates| alternates.contains_key(&alt))
    }

    pub(crate) fn rdpeusb_config_descriptor(&self) -> UsbConfigDesc {
        UsbConfigDesc {
            length: self.length,
            descriptor_type: self.descriptor_type,
            total_length: self.total_length,
            num_interfaces: self.num_interfaces,
            configuration_value: self.configuration_value,
            configuration: self.configuration,
            attributes: self.attributes,
            max_power: self.max_power,
            trailing: self.bytes[9..].to_vec(),
        }
    }

    pub(crate) fn rdpeusb_interfaces(
        &self,
        active_interfaces: &[(u8, u8)],
    ) -> anyhow::Result<Vec<TsUsbdInterfaceInfo>> {
        active_interfaces
            .iter()
            .map(|&(interface, alt)| self.rdpeusb_interface(interface, alt))
            .collect()
    }

    pub(crate) fn rdpeusb_interface(
        &self,
        interface: u8,
        alt: u8,
    ) -> anyhow::Result<TsUsbdInterfaceInfo> {
        let descriptor = self.interface(interface, alt)?;
        ensure!(
            descriptor.endpoints.len() <= 30,
            "RDPEUSB supports at most 30 pipes per interface"
        );
        Ok(TsUsbdInterfaceInfo {
            interface_number: interface,
            alternate_setting: alt,
            ts_usbd_pipe_info: descriptor
                .endpoints
                .iter()
                .map(|endpoint| TsUsbdPipeInfo {
                    max_packet_size: endpoint.max_packet_size,
                    // MS-RDPEUSB section 2.2.9.1.3 notes that the client
                    // ignores MaximumTransferSize. PipeFlags has no
                    // usbredir equivalent, so request the default behavior.
                    max_transfer_size: 0,
                    pipe_flags: 0,
                })
                .collect(),
        })
    }

    pub(crate) fn validate_interface_result(
        &self,
        interface: u8,
        alt: u8,
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
        ensure!(
            result.class == expected.class
                && result.sub_class == expected.subclass
                && result.protocol == expected.protocol,
            "RDPEUSB returned descriptor identity inconsistent with interface {interface} alt {alt}"
        );
        ensure!(
            result.pipes.len() == expected.endpoints.len(),
            "RDPEUSB returned {} pipes for interface {interface} alt {alt}, expected {}",
            result.pipes.len(),
            expected.endpoints.len()
        );

        let expected_endpoints = expected
            .endpoints
            .iter()
            .map(|endpoint| (endpoint.address, endpoint))
            .collect::<BTreeMap<_, _>>();
        let mut returned_endpoints = BTreeMap::new();
        for pipe in &result.pipes {
            ensure!(
                pipe.endpoint_address & 0x70 == 0,
                "RDPEUSB returned invalid endpoint address {:#04x}",
                pipe.endpoint_address
            );
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
                pipe.pipe_type == usbd_pipe_type(endpoint.transfer_type),
                "RDPEUSB returned the wrong pipe type for endpoint {:#04x}",
                pipe.endpoint_address
            );
            ensure!(
                pipe.max_packet_size == endpoint.max_packet_size,
                "RDPEUSB returned max packet size {} for endpoint {:#04x}, expected {}",
                pipe.max_packet_size,
                pipe.endpoint_address,
                endpoint.max_packet_size
            );
            // Do not compare Interval here: USBD_PIPE_INFORMATION can expose
            // a host-stack-normalized polling interval, while usbredir EpInfo
            // is derived from the USB descriptor's bInterval semantics.
        }

        Ok(())
    }

    fn interface(&self, interface: u8, alt: u8) -> anyhow::Result<&InterfaceDescriptor> {
        self.interfaces
            .get(&interface)
            .and_then(|alternates| alternates.get(&alt))
            .ok_or_else(|| {
                anyhow::anyhow!("USB interface {interface} has no alternate setting {alt}")
            })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InterfaceDescriptor {
    number: u8,
    alternate_setting: u8,
    num_endpoints: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: Vec<EndpointDescriptor>,
}

#[derive(Debug, Clone)]
struct EndpointDescriptor {
    address: u8,
    transfer_type: TransferType,
    interval: u8,
    max_packet_size: u16,
    max_streams: u32,
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
        ep_type: [TransferType::Invalid; 32],
        interval: [0; 32],
        interface: [0; 32],
        max_packet_size: [0; 32],
        max_streams: [0; 32],
    };

    let max_packet_size_0 = device.max_packet_size_0()?;
    for index in [0, 16] {
        ep_info.ep_type[index] = TransferType::Control;
        ep_info.max_packet_size[index] = max_packet_size_0;
    }

    let Some(config) = config else {
        return Ok((interface_info, ep_info));
    };

    for (position, &(number, alt_setting)) in active_interfaces.iter().enumerate() {
        let interface = config
            .interfaces
            .get(&number)
            .and_then(|alternates| alternates.get(&alt_setting))
            .ok_or_else(|| {
                anyhow::anyhow!("USB interface {number} has no alternate setting {alt_setting}")
            })?;

        interface_info.interface[position] = number;
        interface_info.interface_class[position] = interface.class;
        interface_info.interface_subclass[position] = interface.subclass;
        interface_info.interface_protocol[position] = interface.protocol;

        for endpoint in &interface.endpoints {
            let index = endpoint_index(endpoint.address);
            ensure!(
                ep_info.ep_type[index] == TransferType::Invalid,
                "duplicate active USB endpoint address {:#04x}",
                endpoint.address
            );
            ep_info.ep_type[index] = endpoint.transfer_type;
            ep_info.interval[index] = usbredir_interval(endpoint, speed);
            ep_info.interface[index] = number;
            ep_info.max_packet_size[index] = endpoint.max_packet_size;
            ep_info.max_streams[index] = endpoint.max_streams;
        }
    }

    Ok((interface_info, ep_info))
}

fn endpoint_index(address: u8) -> usize {
    usize::from(address & 0x0f) + if address & 0x80 == 0 { 0 } else { 16 }
}

fn transfer_type(value: u8) -> TransferType {
    match value {
        0 => TransferType::Control,
        1 => TransferType::Iso,
        2 => TransferType::Bulk,
        3 => TransferType::Interrupt,
        _ => unreachable!("USB endpoint transfer type is two bits"),
    }
}

fn usbd_pipe_type(transfer_type: TransferType) -> UsbdPipeType {
    match transfer_type {
        TransferType::Control => UsbdPipeType::Control,
        TransferType::Iso => UsbdPipeType::Isochronous,
        TransferType::Bulk => UsbdPipeType::Bulk,
        TransferType::Interrupt => UsbdPipeType::Interrupt,
        TransferType::Invalid => unreachable!("invalid endpoints are not stored in descriptors"),
        _ => unreachable!("unsupported endpoint transfer type"),
    }
}

fn actual_max_packet_size(raw: u16) -> u16 {
    let transactions = match (raw >> 11) & 0x03 {
        1 => 2,
        2 => 3,
        _ => 1,
    };
    (raw & 0x07ff) * transactions
}

fn usbredir_interval(endpoint: &EndpointDescriptor, speed: Speed) -> u8 {
    if !matches!(
        endpoint.transfer_type,
        TransferType::Iso | TransferType::Interrupt
    ) {
        return 0;
    }
    if !matches!(speed, Speed::High | Speed::Super) || endpoint.interval == 0 {
        return endpoint.interval;
    }

    let shift = u32::from(endpoint.interval.saturating_sub(1)).min(15);
    (1u32 << shift).min(u32::from(u8::MAX)) as u8
}

#[cfg(test)]
mod tests {
    use ironrdp_rdpeusb::pdu::completion::ts_urb_result::{
        TsUsbdInterfaceInfoResult, TsUsbdPipeInfoResult, UsbdPipeType,
    };

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
    fn builds_complete_rdpeusb_selection_inputs() {
        let descriptor = ConfigurationDescriptor::parse(&CONFIGURATION).unwrap();

        assert_eq!(descriptor.default_alt_settings().unwrap(), vec![(3, 0)]);
        let config = descriptor.rdpeusb_config_descriptor();
        assert_eq!(config.length, 9);
        assert_eq!(config.descriptor_type, 2);
        assert_eq!(config.total_length, 48);
        assert_eq!(config.num_interfaces, 1);
        assert_eq!(config.configuration_value, 2);
        assert_eq!(config.configuration, 7);
        assert_eq!(config.attributes, 0x80);
        assert_eq!(config.max_power, 50);

        let interfaces = descriptor.rdpeusb_interfaces(&[(3, 0)]).unwrap();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].interface_number, 3);
        assert_eq!(interfaces[0].alternate_setting, 0);
        assert_eq!(interfaces[0].ts_usbd_pipe_info.len(), 2);
        assert_eq!(interfaces[0].ts_usbd_pipe_info[0].max_packet_size, 64);
        assert_eq!(interfaces[0].ts_usbd_pipe_info[1].max_packet_size, 8);

        let alternate = descriptor.rdpeusb_interface(3, 1).unwrap();
        assert_eq!(alternate.ts_usbd_pipe_info.len(), 1);
        assert_eq!(alternate.ts_usbd_pipe_info[0].max_packet_size, 128);
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

        descriptor.validate_interface_result(3, 0, &result).unwrap();

        let mut duplicate = result;
        duplicate.pipes[1].endpoint_address = 0x81;
        assert!(descriptor
            .validate_interface_result(3, 0, &duplicate)
            .is_err());
    }
}
