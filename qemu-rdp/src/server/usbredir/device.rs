//! The USB device as QEMU sees it, and its usbredir projection.

use anyhow::{bail, Context as _};
use ironrdp_server::{UsbDeviceHandle, UsbRequestCompletion};
use ironrdp_usb::{
    control::{GetDescriptorRequest, Recipient},
    descriptor::{descriptor_type, DeviceDescriptor},
    value::UsbSpeed,
};
use usbredir_proto::{DeviceConnectInfo, EpInfoData, InterfaceInfoData, Speed, TransferType};

/// `bLength` of a standard USB device descriptor.
const DEVICE_DESCRIPTOR_LENGTH: u16 = 18;

/// Endpoint slots in a usbredir `ep_info` packet: OUT 0-15, then IN 0-15.
const EP_INFO_SLOTS: usize = 32;
const EP_INFO_IN_SLOT_BASE: usize = 16;

pub(super) struct ActiveDevice {
    /// Kept verbatim: usbredir forwards descriptor bytes unchanged, so a quirky
    /// descriptor must survive the round trip byte for byte.
    device_descriptor: Vec<u8>,
    speed: Speed,
    device_connect: DeviceConnectInfo,
    announced: AnnouncedView,
    // The configuration descriptor cache belongs here as well, filled in as
    // QEMU asks for configurations.
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
            device_version_bcd: descriptor.device_release(),
        };
        let ep_info = unconfigured_ep_info(descriptor, speed)?;

        Ok(Self {
            device_descriptor,
            speed,
            device_connect,
            announced: AnnouncedView {
                configuration: 0,
                active_interfaces: Vec::new(),
                interface_info: InterfaceInfoData {
                    interface_count: 0,
                    interface: [0; EP_INFO_SLOTS],
                    interface_class: [0; EP_INFO_SLOTS],
                    interface_subclass: [0; EP_INFO_SLOTS],
                    interface_protocol: [0; EP_INFO_SLOTS],
                },
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
}

/// Endpoint metadata of an unconfigured device: the default control pipe only.
fn unconfigured_ep_info(
    descriptor: DeviceDescriptor<'_>,
    speed: Speed,
) -> anyhow::Result<EpInfoData> {
    let max_packet_size_0 = descriptor
        .endpoint_zero_max_packet_size(usb_speed(speed)?)
        .context("interpret usb bMaxPacketSize0")?;

    let mut ep_info = EpInfoData {
        ep_type: [TransferType::Invalid; EP_INFO_SLOTS],
        interval: [0; EP_INFO_SLOTS],
        interface: [0; EP_INFO_SLOTS],
        max_packet_size: [0; EP_INFO_SLOTS],
        max_streams: [0; EP_INFO_SLOTS],
    };

    // The default control pipe occupies both direction slots of endpoint zero.
    for slot in [0, EP_INFO_IN_SLOT_BASE] {
        ep_info.ep_type[slot] = TransferType::Control;
        ep_info.max_packet_size[slot] = max_packet_size_0;
    }

    Ok(ep_info)
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
