use super::*;
use anyhow::{bail, ensure, Context as _};
use ironrdp_rdpeusb::{
    io::{TransferInPacket, TsUrbInKind, TsUrbInPacket, UrbFunction},
    pdu::{
        sink::DeviceSpeed,
        usb_dev::ts_urb::{
            TsUrbControlDescRequest, TsUrbControlGetConfigRequest, TsUrbControlGetInterfaceRequest,
        },
    },
};
use usbredir_proto::{DeviceConnectInfo, EpInfoData, InterfaceInfoData, Speed};

use super::proto::descriptor::{self, ConfigurationDescriptor, DeviceDescriptor};

pub(super) struct RedirTask {
    usbredir: UsbRedir<RdpUsb>,
    rdp_handle: UsbDeviceHandle,
    rx: UnboundedReceiver<RedirEvent>,
}

/// Initial device announcement data.
///
/// Ref: usbredir protocol v0.7, `usb_redir_device_connect`: send `ep_info`,
/// then `interface_info`, and only then `device_connect`.
pub(super) struct DeviceData {
    pub(super) device_connect: DeviceConnectInfo,
    pub(super) interface_info: InterfaceInfoData,
    pub(super) ep_info: EpInfoData,
    pub(super) configuration: u8,
}

impl RedirTask {
    pub(super) fn new(
        usbredir: UsbRedir<RdpUsb>,
        rdp_handle: UsbDeviceHandle,
        rx: UnboundedReceiver<RedirEvent>,
    ) -> Self {
        Self {
            usbredir,
            rdp_handle,
            rx,
        }
    }

    async fn bootstrap(&mut self, announce: DeviceAnnounce) -> anyhow::Result<DeviceData> {
        let device_descriptor = self
            .get_descriptor(
                descriptor::DEVICE_DESCRIPTOR_TYPE,
                0,
                descriptor::DEVICE_DESCRIPTOR_SIZE,
            )
            .await
            .context("query USB device descriptor")?;
        let device_descriptor = DeviceDescriptor::parse(&device_descriptor)?;

        let configuration = self.get_configuration().await?;
        let config_descriptor = if configuration == 0 {
            None
        } else {
            Some(
                self.get_active_config_descriptor(
                    device_descriptor.num_configurations,
                    configuration,
                )
                .await?,
            )
        };

        let active_interfaces = if let Some(config_descriptor) = &config_descriptor {
            let mut active = Vec::with_capacity(config_descriptor.interfaces.len());
            for interface in config_descriptor.interfaces.keys().copied() {
                let alt_setting = self.get_interface(interface).await?;
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
            DeviceSpeed::HighSpeed => Speed::High,
            DeviceSpeed::FullSpeed => Speed::Full,
        };
        let (interface_info, ep_info) = descriptor::usbredir_config_info(
            &device_descriptor,
            config_descriptor.as_ref(),
            &active_interfaces,
            speed,
        )?;

        Ok(DeviceData {
            // Ref: usbredir protocol v0.7, `usb_redir_device_connect`.
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
            configuration,
        })
    }

    async fn get_active_config_descriptor(
        &mut self,
        num_configurations: u8,
        active_configuration: u8,
    ) -> anyhow::Result<ConfigurationDescriptor> {
        for index in 0..num_configurations {
            let header = self
                .get_descriptor(
                    descriptor::CONFIG_DESCRIPTOR_TYPE,
                    index,
                    descriptor::CONFIG_DESCRIPTOR_SIZE,
                )
                .await
                .with_context(|| format!("query USB configuration descriptor {index} header"))?;
            let (total_length, configuration_value) =
                ConfigurationDescriptor::parse_header(&header)?;

            if configuration_value == active_configuration {
                let bytes = self
                    .get_descriptor(descriptor::CONFIG_DESCRIPTOR_TYPE, index, total_length)
                    .await
                    .with_context(|| format!("query USB configuration descriptor {index}"))?;
                return ConfigurationDescriptor::parse(&bytes);
            }
        }

        bail!("active USB configuration {active_configuration} has no descriptor")
    }

    async fn get_descriptor(
        &mut self,
        desc_type: u8,
        index: u8,
        length: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let output_buffer_size =
            u32::try_from(length).context("USB descriptor request is too large")?;
        self.transfer_in(TransferInPacket {
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

    async fn get_configuration(&mut self) -> anyhow::Result<u8> {
        let data = self
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

    async fn get_interface(&mut self, interface: u8) -> anyhow::Result<u8> {
        let data = self
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

    async fn transfer_in(&mut self, packet: TransferInPacket) -> anyhow::Result<Vec<u8>> {
        let sent = self.rdp_handle.transfer_in_request(packet).await?;
        ensure!(
            sent.expects_completion,
            "RDPEUSB transfer-in unexpectedly has no completion"
        );

        loop {
            let event = self.rx.recv().await.ok_or_else(|| {
                anyhow::anyhow!(
                    "USB device session dropped while waiting for transfer-in completion"
                )
            })?;

            match event {
                RedirEvent::TransferInCompleted {
                    request_id,
                    completion,
                } if request_id == sent.request_id => {
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
                    return Ok(completion.output_buffer);
                }
                other => tracing::debug!(?other, "ignoring unrelated USB event while booting"),
            }
        }
    }

    pub(super) async fn start(mut self, announce: DeviceAnnounce) -> anyhow::Result<()> {
        let key = announce.device_instance_id.clone();

        // communicate with RDP client to get enough data for creating device via usbredir.
        let device_data = loop {
            let event = self
                .rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("session dropped"))?;

            // TODO: ironrdp processor receives REGISTER_REQUEST_CALLBACK should emit a callback
            if !matches!(event, RedirEvent::Ready) {
                tracing::debug!("received {event:?} while booting");
                continue;
            }
            break self.bootstrap(announce).await?;
        };

        let device = RdpUsbRedirDevice::new(device_data, self.rdp_handle, self.rx);

        self.usbredir.connect_device(key, device).await?;
        Ok(())
    }
}
