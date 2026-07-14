//! USB descriptor parsing and conversion into usbredir protocol metadata.

use std::collections::BTreeMap;

use anyhow::{bail, ensure};
use usbredir_proto::{EpInfoData, InterfaceInfoData, Speed, TransferType};

pub(crate) const DEVICE_DESCRIPTOR_TYPE: u8 = 0x01;
pub(crate) const CONFIG_DESCRIPTOR_TYPE: u8 = 0x02;
const INTERFACE_DESCRIPTOR_TYPE: u8 = 0x04;
const ENDPOINT_DESCRIPTOR_TYPE: u8 = 0x05;
const SS_ENDPOINT_COMPANION_DESCRIPTOR_TYPE: u8 = 0x30;

pub(crate) const DEVICE_DESCRIPTOR_SIZE: usize = 18;
pub(crate) const CONFIG_DESCRIPTOR_SIZE: usize = 9;

#[derive(Debug)]
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
}

#[derive(Debug)]
pub(crate) struct ConfigurationDescriptor {
    pub(crate) interfaces: BTreeMap<u8, BTreeMap<u8, InterfaceDescriptor>>,
}

impl ConfigurationDescriptor {
    pub(crate) fn parse_header(bytes: &[u8]) -> anyhow::Result<(usize, u8)> {
        ensure!(
            bytes.len() >= CONFIG_DESCRIPTOR_SIZE,
            "USB configuration descriptor header is truncated"
        );
        ensure!(
            usize::from(bytes[0]) >= CONFIG_DESCRIPTOR_SIZE,
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
        let (total_length, _configuration_value) = Self::parse_header(bytes)?;
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
                        class: bytes[offset + 5],
                        subclass: bytes[offset + 6],
                        protocol: bytes[offset + 7],
                        endpoints: Vec::new(),
                    };
                    let key = (interface.number, interface.alternate_setting);
                    interfaces
                        .entry(key.0)
                        .or_default()
                        .insert(key.1, interface);
                    current_interface = Some(key);
                    current_endpoint = None;
                }
                ENDPOINT_DESCRIPTOR_TYPE if length >= 7 => {
                    let Some((number, alt)) = current_interface else {
                        bail!("USB endpoint descriptor appears before an interface descriptor");
                    };
                    let endpoint = EndpointDescriptor {
                        address: bytes[offset + 2],
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

        Ok(Self { interfaces })
    }
}

#[derive(Debug)]
pub(crate) struct InterfaceDescriptor {
    number: u8,
    alternate_setting: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: Vec<EndpointDescriptor>,
}

#[derive(Debug)]
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
