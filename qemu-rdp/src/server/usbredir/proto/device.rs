//! RDPEUSB device discovery and usbredir announcement construction.

use anyhow::{bail, ensure, Context as _};
use ironrdp_rdpeusb::{
    io::{DeviceAnnounce, TransferInPacket, TsUrbInKind, TsUrbInPacket, UrbFunction},
    pdu::{
        sink::DeviceSpeed,
        usb_dev::ts_urb::{
            TsUrbControlDescRequest, TsUrbControlGetConfigRequest, TsUrbControlGetInterfaceRequest,
        },
    },
};
use usbredir_proto::{DeviceConnectInfo, EpInfoData, InterfaceInfoData, Speed};

use super::super::rdpeusb_io::RdpeusbDeviceIo;
use super::descriptor::{self, ConfigurationDescriptor, DeviceDescriptor};

/// Initial device state needed by the usbredir bridge.
pub(super) struct PreparedDevice {
    pub(super) source: DeviceAnnounce,
    pub(super) announcement: DeviceAnnouncement,
    pub(super) configuration: u8,
}

/// Metadata sent before usbredir makes the device visible to the guest.
///
/// Ref: usbredir protocol v0.7, `usb_redir_device_connect`: send `ep_info`,
/// then `interface_info`, and only then `device_connect`.
pub(super) struct DeviceAnnouncement {
    pub(super) device_connect: DeviceConnectInfo,
    pub(super) interface_info: InterfaceInfoData,
    pub(super) ep_info: EpInfoData,
}

pub(super) async fn prepare_device(
    announce: DeviceAnnounce,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<PreparedDevice> {
    let device_descriptor = get_descriptor(
        io,
        descriptor::DEVICE_DESCRIPTOR_TYPE,
        0,
        descriptor::DEVICE_DESCRIPTOR_SIZE,
    )
    .await
    .context("query USB device descriptor")?;
    let device_descriptor = DeviceDescriptor::parse(&device_descriptor)?;

    let configuration = get_configuration(io).await?;
    let config_descriptor = if configuration == 0 {
        None
    } else {
        Some(
            get_active_config_descriptor(io, device_descriptor.num_configurations, configuration)
                .await?,
        )
    };

    let active_interfaces = if let Some(config_descriptor) = &config_descriptor {
        let mut active = Vec::with_capacity(config_descriptor.interfaces.len());
        for interface in config_descriptor.interfaces.keys().copied() {
            let alt_setting = get_interface(io, interface).await?;
            active.push((interface, alt_setting));
        }
        active
    } else {
        Vec::new()
    };

    // Ref: MS-RDPEUSB `ADD_DEVICE` / `UsbDeviceCaps.DeviceSpeed` and
    // usbredir v0.7 `usb_redir_device_connect`.
    //
    // TODO: RDPEUSB exposes only the Full/High distinction; bcdUSB alone
    // does not prove SuperSpeed.
    let speed = match announce.usb_device_caps.device_speed {
        DeviceSpeed::HIGH_SPEED => Speed::High,
        DeviceSpeed::FULL_SPEED => Speed::Full,
        _ => Speed::Unknown,
    };
    let (interface_info, ep_info) = descriptor::usbredir_config_info(
        &device_descriptor,
        config_descriptor.as_ref(),
        &active_interfaces,
        speed,
    )?;

    Ok(PreparedDevice {
        source: announce,
        // Ref: usbredir protocol v0.7, `usb_redir_device_connect`.
        announcement: DeviceAnnouncement {
            device_connect: DeviceConnectInfo {
                speed,
                device_class: device_descriptor.device_class,
                device_subclass: device_descriptor.device_subclass,
                device_protocol: device_descriptor.device_protocol,
                vendor_id: device_descriptor.vendor_id,
                product_id: device_descriptor.product_id,
                device_version_bcd: device_descriptor.device_version,
            },
            interface_info,
            ep_info,
        },
        configuration,
    })
}

async fn get_active_config_descriptor(
    io: &mut RdpeusbDeviceIo,
    num_configurations: u8,
    active_configuration: u8,
) -> anyhow::Result<ConfigurationDescriptor> {
    for index in 0..num_configurations {
        let header = get_descriptor(
            io,
            descriptor::CONFIG_DESCRIPTOR_TYPE,
            index,
            descriptor::CONFIG_DESCRIPTOR_SIZE,
        )
        .await
        .with_context(|| format!("query USB configuration descriptor {index} header"))?;
        let (total_length, configuration_value) = ConfigurationDescriptor::parse_header(&header)?;

        if configuration_value == active_configuration {
            let bytes = get_descriptor(io, descriptor::CONFIG_DESCRIPTOR_TYPE, index, total_length)
                .await
                .with_context(|| format!("query USB configuration descriptor {index}"))?;
            return ConfigurationDescriptor::parse(&bytes);
        }
    }

    bail!("active USB configuration {active_configuration} has no descriptor")
}

async fn get_descriptor(
    io: &mut RdpeusbDeviceIo,
    desc_type: u8,
    index: u8,
    length: usize,
) -> anyhow::Result<Vec<u8>> {
    let output_buffer_size =
        u32::try_from(length).context("USB descriptor request is too large")?;
    io.transfer_in(TransferInPacket {
        ts_urb: TsUrbInPacket {
            kind: TsUrbInKind::CtlDescReq(TsUrbControlDescRequest {
                index,
                desc_type,
                lang_id: 0,
            }),
            func: UrbFunction::URB_FUNCTION_GET_DESCRIPTOR_FROM_DEVICE,
        },
        output_buffer_size,
    })
    .await
}

async fn get_configuration(io: &mut RdpeusbDeviceIo) -> anyhow::Result<u8> {
    let data = io
        .transfer_in(TransferInPacket {
            ts_urb: TsUrbInPacket {
                kind: TsUrbInKind::CtlGetConfig(TsUrbControlGetConfigRequest),
                func: UrbFunction::URB_FUNCTION_GET_CONFIGURATION,
            },
            output_buffer_size: 1,
        })
        .await
        .context("query active USB configuration")?;

    ensure!(
        data.len() == 1,
        "GET_CONFIGURATION returned {} bytes",
        data.len()
    );
    Ok(data[0])
}

async fn get_interface(io: &mut RdpeusbDeviceIo, interface: u8) -> anyhow::Result<u8> {
    let data = io
        .transfer_in(TransferInPacket {
            ts_urb: TsUrbInPacket {
                kind: TsUrbInKind::CtlGetIface(TsUrbControlGetInterfaceRequest {
                    interface: u16::from(interface),
                }),
                func: UrbFunction::URB_FUNCTION_GET_INTERFACE,
            },
            output_buffer_size: 1,
        })
        .await
        .with_context(|| format!("query alternate setting for USB interface {interface}"))?;

    ensure!(
        data.len() == 1,
        "GET_INTERFACE returned {} bytes",
        data.len()
    );
    Ok(data[0])
}
