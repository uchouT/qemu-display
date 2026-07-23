//! RDPEUSB device discovery and selection operations.

use anyhow::{bail, ensure, Context as _};
use ironrdp_rdpeusb::{
    io::{
        DeviceAnnounce, TransferInCompletionResult, TransferInPacket, TsUrbInKind, TsUrbInPacket,
        UrbFunction,
    },
    pdu::{
        completion::ts_urb_result::{
            TsUrbResultPayload, TsUrbSelectConfigResult, TsUrbSelectInterfaceResult,
        },
        sink::DeviceSpeed,
        usb_dev::ts_urb::{TsUrbControlDescRequest, TsUrbSelectConfig, TsUrbSelectInterface},
    },
};
use usbredir_proto::Speed;

use super::super::rdpeusb_io::RdpeusbDeviceIo;
use super::descriptor::{self, ConfigurationDescriptor};

/// Minimum physical-device identity needed to create the guest-visible model.
pub(super) struct PreparedDevice {
    pub(super) source: DeviceAnnounce,
    pub(super) device_descriptor_bytes: Vec<u8>,
    pub(super) speed: Speed,
}

pub(super) async fn prepare_device(
    announce: DeviceAnnounce,
    io: &mut RdpeusbDeviceIo,
) -> anyhow::Result<PreparedDevice> {
    let device_descriptor_bytes = get_descriptor(
        io,
        descriptor::DEVICE_DESCRIPTOR_TYPE,
        0,
        descriptor::DEVICE_DESCRIPTOR_SIZE,
    )
    .await
    .context("query USB device descriptor")?;

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
    Ok(PreparedDevice {
        source: announce,
        device_descriptor_bytes,
        speed,
    })
}

pub(super) async fn get_config_descriptor(
    io: &mut RdpeusbDeviceIo,
    indices: &[u8],
    configuration: u8,
) -> anyhow::Result<Option<(u8, ConfigurationDescriptor)>> {
    for &index in indices {
        let header = get_descriptor(
            io,
            descriptor::CONFIG_DESCRIPTOR_TYPE,
            index,
            descriptor::CONFIG_DESCRIPTOR_SIZE,
        )
        .await
        .with_context(|| format!("query USB configuration descriptor {index} header"))?;
        let (total_length, configuration_value) = ConfigurationDescriptor::parse_header(&header)?;

        if configuration_value == configuration {
            let bytes = get_descriptor(io, descriptor::CONFIG_DESCRIPTOR_TYPE, index, total_length)
                .await
                .with_context(|| format!("query USB configuration descriptor {index}"))?;
            let descriptor = ConfigurationDescriptor::parse(&bytes)?;
            ensure!(
                descriptor.configuration_value() == configuration,
                "USB configuration descriptor {index} changed between header and full reads"
            );
            return Ok(Some((index, descriptor)));
        }
    }

    Ok(None)
}

pub(super) async fn select_configuration(
    io: &mut RdpeusbDeviceIo,
    request: TsUrbSelectConfig,
) -> anyhow::Result<TsUrbSelectConfigResult> {
    let completion = io
        .wait_transfer_in(TransferInPacket {
            ts_urb: TsUrbInPacket {
                kind: TsUrbInKind::SelectConfig(request),
                func: UrbFunction::URB_FUNCTION_SELECT_CONFIGURATION,
            },
            output_buffer_size: 0,
        })
        .await
        .context("select USB configuration")?;
    ensure_successful_transfer_in(&completion)?;
    ensure!(
        completion.output_buffer.is_empty(),
        "SELECT_CONFIGURATION unexpectedly returned output data"
    );
    let Some(TsUrbResultPayload::SelectConfig(result)) = completion.ts_urb_result.payload else {
        bail!("SELECT_CONFIGURATION completion has no select-configuration result")
    };
    Ok(result)
}

pub(super) async fn select_interface(
    io: &mut RdpeusbDeviceIo,
    request: TsUrbSelectInterface,
) -> anyhow::Result<TsUrbSelectInterfaceResult> {
    let completion = io
        .wait_transfer_in(TransferInPacket {
            ts_urb: TsUrbInPacket {
                kind: TsUrbInKind::SelectIface(request),
                func: UrbFunction::URB_FUNCTION_SELECT_INTERFACE,
            },
            output_buffer_size: 0,
        })
        .await
        .context("select USB interface")?;
    ensure_successful_transfer_in(&completion)?;
    ensure!(
        completion.output_buffer.is_empty(),
        "SELECT_INTERFACE unexpectedly returned output data"
    );
    let Some(TsUrbResultPayload::SelectIface(result)) = completion.ts_urb_result.payload else {
        bail!("SELECT_INTERFACE completion has no select-interface result")
    };
    Ok(result)
}

async fn get_descriptor(
    io: &mut RdpeusbDeviceIo,
    desc_type: u8,
    index: u8,
    length: usize,
) -> anyhow::Result<Vec<u8>> {
    let output_buffer_size =
        u32::try_from(length).context("USB descriptor request is too large")?;
    let completion = io
        .wait_transfer_in(TransferInPacket {
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
        .await?;
    successful_transfer_in_output(completion)
}

fn successful_transfer_in_output(
    completion: TransferInCompletionResult,
) -> anyhow::Result<Vec<u8>> {
    ensure_successful_transfer_in(&completion)?;
    Ok(completion.output_buffer)
}

fn ensure_successful_transfer_in(completion: &TransferInCompletionResult) -> anyhow::Result<()> {
    ensure!(
        completion.hresult == 0,
        "RDPEUSB transfer-in failed with HRESULT {:#010x}",
        completion.hresult
    );
    ensure!(
        completion.ts_urb_result.header.usbd_status == 0,
        "USB transfer-in failed with USBD status {:#010x}",
        completion.ts_urb_result.header.usbd_status
    );
    Ok(())
}
