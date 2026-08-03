//! Canonical USB device state and RDPEUSB handle bindings.

use std::collections::{BTreeMap, HashMap};

use anyhow::{ensure, Context as _};
use ironrdp_rdpeusb::{
    io::{DeviceAnnounce, TransferInPacket},
    pdu::{
        completion::ts_urb_result::{
            TsUrbSelectConfigResult, TsUrbSelectInterfaceResult, UsbdPipeType,
        },
        utils::{ConfigHandle, PipeHandle},
    },
    usb as rdpeusb,
};
use ironrdp_usb::value::{InterfaceSelection as UsbInterfaceSelection, UsbSpeed};
use usbredir_proto::{DeviceConnectInfo, Endpoint, EpInfoData, InterfaceInfoData, Speed};

use super::{
    descriptor::{self, ConfigurationDescriptor, DeviceDescriptor},
    device::PreparedDevice,
};

/// Canonical guest-visible device state, kept separate from the opaque handles
/// that make its current RDPEUSB data path usable.
pub(super) struct ActiveDevice {
    // Retained for retract/disconnect handling once device lifecycle support is
    // completed.
    #[allow(dead_code)]
    pub(super) source: DeviceAnnounce,
    model: DeviceModel,
    announced: AnnouncedView,
    binding: BindingState,
    next_generation: u64,
}

struct DeviceModel {
    device_descriptor_bytes: Vec<u8>,
    device_descriptor: DeviceDescriptor,
    speed: Speed,
    device_connect: DeviceConnectInfo,
    /// Complete configuration descriptor sets keyed by descriptor index.
    configurations: BTreeMap<u8, ConfigurationDescriptor>,
}

struct AnnouncedView {
    selection: DeviceSelection,
    interface_info: InterfaceInfoData,
    ep_info: EpInfoData,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceSelection {
    configuration: u8,
    active_interfaces: Vec<(u8, u8)>,
}

/// RDPEUSB bindings are intentionally distinct from the last state announced
/// to QEMU. The guest-visible device starts unconfigured; only a successful
/// usbredir SetConfiguration installs configuration, interface, and pipe
/// handles.
#[allow(dead_code)]
pub(super) enum BindingState {
    Unconfigured { generation: u64 },
    Configured(ConfigurationBinding),
    Unknown { generation: u64 },
}

#[allow(dead_code)]
pub(super) struct ConfigurationBinding {
    configuration: u8,
    config_handle: ConfigHandle,
    generation: u64,
    interfaces: BTreeMap<u8, InterfaceBindingState>,
}

#[allow(dead_code)]
pub(super) enum InterfaceBindingState {
    Bound(InterfaceBinding),
    Unknown {
        alternate_setting: u8,
        generation: u64,
    },
}

#[allow(dead_code)]
pub(super) struct InterfaceBinding {
    alternate_setting: u8,
    interface_handle: u32,
    generation: u64,
    pipes: HashMap<Endpoint, PipeBinding>,
}

#[allow(dead_code)]
pub(super) struct PipeBinding {
    handle: PipeHandle,
    pipe_type: UsbdPipeType,
    max_packet_size: u16,
    interval: u8,
    max_transfer_size: u32,
    flags: u32,
}

/// A fully validated target selection. It is built without changing live
/// state, submitted to RDPEUSB, then committed atomically with returned
/// handles.
pub(super) struct ConfigurationPlan {
    selection: DeviceSelection,
    descriptor: Option<ConfigurationDescriptor>,
    usb_speed: UsbSpeed,
    interface_info: InterfaceInfoData,
    ep_info: EpInfoData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConfigurationSelectionToken {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InterfaceSelectionToken {
    interface: u8,
    generation: u64,
}

impl ActiveDevice {
    pub(super) fn from_prepared(prepared: PreparedDevice) -> anyhow::Result<Self> {
        let PreparedDevice {
            source,
            device_descriptor_bytes,
            speed,
        } = prepared;
        ensure!(
            device_descriptor_bytes.len() == descriptor::DEVICE_DESCRIPTOR_SIZE,
            "cached USB device descriptor has unexpected length {}",
            device_descriptor_bytes.len()
        );
        let device_descriptor = DeviceDescriptor::parse(&device_descriptor_bytes)?;
        let device_connect = DeviceConnectInfo {
            speed,
            device_class: device_descriptor.device_class,
            device_subclass: device_descriptor.device_subclass,
            device_protocol: device_descriptor.device_protocol,
            vendor_id: device_descriptor.vendor_id,
            product_id: device_descriptor.product_id,
            device_version_bcd: device_descriptor.device_version,
        };
        let (interface_info, ep_info) =
            descriptor::usbredir_config_info(&device_descriptor, None, &[], speed)?;

        // The bridge does not import the client's incidental configuration or
        // alternate settings. QEMU observes configuration zero until its own
        // SetConfiguration transaction succeeds and supplies fresh handles.
        Ok(Self {
            source,
            model: DeviceModel {
                device_descriptor_bytes,
                device_descriptor,
                speed: device_connect.speed,
                device_connect,
                configurations: BTreeMap::new(),
            },
            announced: AnnouncedView {
                selection: DeviceSelection {
                    configuration: 0,
                    active_interfaces: Vec::new(),
                },
                interface_info,
                ep_info,
                revision: 0,
            },
            binding: BindingState::Unconfigured { generation: 0 },
            next_generation: 0,
        })
    }

    pub(super) fn configuration(&self) -> u8 {
        self.announced.selection.configuration
    }

    pub(super) fn alt_setting(&self, interface: u8) -> Option<u8> {
        self.announced
            .selection
            .active_interfaces
            .iter()
            .find_map(|&(number, alt)| (number == interface).then_some(alt))
    }

    pub(super) fn num_configurations(&self) -> u8 {
        self.model.device_descriptor.num_configurations()
    }

    pub(super) fn has_configuration(&self, configuration: u8) -> bool {
        configuration == 0 || self.configuration_descriptor(configuration).is_some()
    }

    fn configuration_descriptor(&self, configuration: u8) -> Option<&ConfigurationDescriptor> {
        self.model
            .configurations
            .values()
            .find(|descriptor| descriptor.configuration_value() == configuration)
    }

    pub(super) fn device_descriptor_bytes(&self) -> &[u8] {
        &self.model.device_descriptor_bytes
    }

    pub(super) fn configuration_descriptor_by_index(
        &self,
        index: u8,
    ) -> Option<&ConfigurationDescriptor> {
        self.model.configurations.get(&index)
    }

    /// Installs a configuration descriptor exactly once for this device
    /// insertion. The entire cache is discarded with `ActiveDevice` on
    /// disconnect.
    pub(super) fn insert_configuration_descriptor(
        &mut self,
        index: u8,
        descriptor: ConfigurationDescriptor,
    ) -> anyhow::Result<()> {
        ensure!(
            index < self.num_configurations(),
            "USB configuration descriptor index {index} exceeds bNumConfigurations {}",
            self.num_configurations()
        );
        let configuration = descriptor.configuration_value();
        ensure!(configuration != 0, "configuration zero has no descriptor");
        ensure!(
            !self.model.configurations.contains_key(&index),
            "USB configuration descriptor index {index} is already cached"
        );
        ensure!(
            self.model
                .configurations
                .values()
                .all(|cached| cached.configuration_value() != configuration),
            "USB configuration value {configuration} is advertised by multiple descriptor indices"
        );
        self.model.configurations.insert(index, descriptor);
        Ok(())
    }

    pub(super) fn has_alt_setting(&self, interface: u8, alt: u8) -> bool {
        self.configuration_descriptor(self.configuration())
            .is_some_and(|descriptor| descriptor.contains_alt_setting(interface, alt))
    }

    pub(super) fn plan_configuration(
        &self,
        configuration: u8,
    ) -> anyhow::Result<ConfigurationPlan> {
        if configuration == 0 {
            return ConfigurationPlan::new(
                &self.model.device_descriptor,
                self.model.speed,
                DeviceSelection {
                    configuration: 0,
                    active_interfaces: Vec::new(),
                },
                None,
            );
        }

        let descriptor = self
            .configuration_descriptor(configuration)
            .with_context(|| format!("USB configuration {configuration} is not cached"))?
            .clone();
        let active_interfaces = descriptor.default_alt_settings()?;
        ConfigurationPlan::new(
            &self.model.device_descriptor,
            self.model.speed,
            DeviceSelection {
                configuration,
                active_interfaces,
            },
            Some(descriptor),
        )
    }

    pub(super) fn plan_alt_setting(
        &self,
        interface: u8,
        alt: u8,
    ) -> anyhow::Result<ConfigurationPlan> {
        ensure!(
            self.configuration() != 0,
            "cannot select an alternate setting while unconfigured"
        );
        let descriptor = self
            .configuration_descriptor(self.configuration())
            .context("active USB configuration is not cached")?
            .clone();
        ensure!(
            descriptor.contains_alt_setting(interface, alt),
            "USB interface {interface} has no alternate setting {alt}"
        );

        let mut selection = self.announced.selection.clone();
        let selected_alt = selection
            .active_interfaces
            .iter_mut()
            .find_map(|(number, selected_alt)| (*number == interface).then_some(selected_alt))
            .with_context(|| format!("USB interface {interface} is not active"))?;
        *selected_alt = alt;

        ConfigurationPlan::new(
            &self.model.device_descriptor,
            self.model.speed,
            selection,
            Some(descriptor),
        )
    }

    pub(super) fn device_connect(&self) -> DeviceConnectInfo {
        self.model.device_connect.clone()
    }

    pub(super) fn metadata(&self) -> (EpInfoData, InterfaceInfoData) {
        (
            self.announced.ep_info.clone(),
            self.announced.interface_info.clone(),
        )
    }

    #[allow(dead_code)] // Reserved for endpoint/pipe lookups in the data plane.
    pub(super) fn configuration_handle(&self) -> anyhow::Result<ConfigHandle> {
        let handle = self.configuration_handle_for_interface_selection()?;
        let BindingState::Configured(binding) = &self.binding else {
            unreachable!("selection handle requires a configured binding")
        };
        ensure!(
            self.announced
                .selection
                .active_interfaces
                .iter()
                .all(|&(interface, alt)| {
                    matches!(
                        binding.interfaces.get(&interface),
                        Some(InterfaceBindingState::Bound(bound))
                            if bound.alternate_setting == alt
                    )
                }),
            "RDPEUSB configuration binding does not match announced USB state"
        );
        Ok(handle)
    }

    /// Resolves an endpoint against the handles installed for the active
    /// configuration and alternate settings.
    ///
    /// Endpoint addresses are unique within one active USB configuration, so
    /// finding the same address in two interface bindings indicates an invalid
    /// RDPEUSB selection result rather than an ambiguous routing choice.
    pub(super) fn bulk_pipe_handle(&self, endpoint: Endpoint) -> anyhow::Result<PipeHandle> {
        // Besides checking that the device is configured, this verifies that
        // every live interface binding still matches the state announced to
        // QEMU. Data must never be submitted through a stale pipe handle.
        self.configuration_handle()?;
        let BindingState::Configured(binding) = &self.binding else {
            unreachable!("configuration_handle requires a configured binding")
        };

        let mut matching_pipe = None;
        for interface in binding.interfaces.values() {
            let InterfaceBindingState::Bound(interface) = interface else {
                unreachable!("configuration_handle rejects unknown interface bindings")
            };
            let Some(pipe) = interface.pipes.get(&endpoint) else {
                continue;
            };
            ensure!(
                matching_pipe.replace(pipe).is_none(),
                "endpoint {endpoint} is bound by multiple active USB interfaces"
            );
        }

        let pipe = matching_pipe.with_context(|| {
            format!("endpoint {endpoint} has no pipe in the active USB configuration")
        })?;
        ensure!(
            pipe.pipe_type == UsbdPipeType::Bulk,
            "endpoint {endpoint} is {:?}, not bulk",
            pipe.pipe_type
        );
        Ok(pipe.handle)
    }

    /// Returns the configuration handle usable by SELECT_INTERFACE.
    ///
    /// An interface-level `Unknown` state means a previous SELECT_INTERFACE
    /// did not complete reliably. Its configuration handle remains the one
    /// returned by the last successful SELECT_CONFIGURATION and can be used
    /// to retry that interface selection. Data-plane access remains stricter
    /// through [`Self::configuration_handle`].
    pub(super) fn configuration_handle_for_interface_selection(
        &self,
    ) -> anyhow::Result<ConfigHandle> {
        let BindingState::Configured(binding) = &self.binding else {
            anyhow::bail!("active USB configuration has no RDPEUSB binding")
        };
        ensure!(
            binding.configuration == self.configuration()
                && binding.interfaces.len() == self.announced.selection.active_interfaces.len()
                && self
                    .announced
                    .selection
                    .active_interfaces
                    .iter()
                    .all(|&(interface, alt)| {
                        matches!(
                            binding.interfaces.get(&interface),
                            Some(InterfaceBindingState::Bound(bound))
                                if bound.alternate_setting == alt
                        ) || matches!(
                            binding.interfaces.get(&interface),
                            Some(InterfaceBindingState::Unknown {
                                alternate_setting,
                                ..
                            }) if *alternate_setting == alt
                        )
                    }),
            "RDPEUSB configuration binding does not match announced USB state"
        );
        Ok(binding.config_handle)
    }

    pub(super) fn begin_configuration_selection(
        &mut self,
    ) -> anyhow::Result<ConfigurationSelectionToken> {
        let generation = self.allocate_generation()?;
        // TODO: Cancel pending data packets and stop streams before replacing
        // the configuration. Those registries will live beside this state.
        self.binding = BindingState::Unknown { generation };
        Ok(ConfigurationSelectionToken { generation })
    }

    pub(super) fn validate_configuration_result(
        &self,
        plan: &ConfigurationPlan,
        result: TsUrbSelectConfigResult,
        token: ConfigurationSelectionToken,
    ) -> anyhow::Result<BindingState> {
        if plan.selection.configuration == 0 {
            ensure!(
                result.interface.is_empty(),
                "unconfigure returned unexpected RDPEUSB interface bindings"
            );
            return Ok(BindingState::Unconfigured {
                generation: token.generation,
            });
        }

        let descriptor = plan
            .descriptor
            .as_ref()
            .context("configured selection plan has no descriptor")?;
        ensure!(
            result.interface.len() == plan.selection.active_interfaces.len(),
            "SELECT_CONFIGURATION returned {} interfaces, expected {}",
            result.interface.len(),
            plan.selection.active_interfaces.len()
        );

        let expected = plan
            .selection
            .active_interfaces
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>();
        let mut interfaces = BTreeMap::new();
        for interface in result.interface {
            let alt = expected
                .get(&interface.interface_number)
                .copied()
                .with_context(|| {
                    format!(
                        "SELECT_CONFIGURATION returned unexpected interface {}",
                        interface.interface_number
                    )
                })?;
            descriptor.validate_interface_result(
                interface.interface_number,
                alt,
                plan.usb_speed,
                &interface,
            )?;
            let number = interface.interface_number;
            ensure!(
                interfaces
                    .insert(
                        number,
                        InterfaceBindingState::Bound(InterfaceBinding::from_result(
                            interface,
                            token.generation,
                        )?),
                    )
                    .is_none(),
                "SELECT_CONFIGURATION returned duplicate interface {number}"
            );
        }

        Ok(BindingState::Configured(ConfigurationBinding {
            configuration: plan.selection.configuration,
            config_handle: result.config_handle,
            generation: token.generation,
            interfaces,
        }))
    }

    pub(super) fn commit_configuration(
        &mut self,
        plan: ConfigurationPlan,
        binding: BindingState,
        token: ConfigurationSelectionToken,
    ) -> anyhow::Result<()> {
        self.validate_configuration_commit(&plan, &binding, token)?;
        ensure!(
            matches!(
                (&binding, plan.selection.configuration),
                (BindingState::Unconfigured { .. }, 0) | (BindingState::Configured(_), 1..=u8::MAX)
            ),
            "RDPEUSB binding does not match configuration plan"
        );
        let revision = self
            .announced
            .revision
            .checked_add(1)
            .context("usbredir announcement revision exhausted")?;
        self.announced = plan.into_announced(revision);
        self.binding = binding;
        Ok(())
    }

    pub(super) fn begin_interface_selection(
        &mut self,
        interface: u8,
    ) -> anyhow::Result<InterfaceSelectionToken> {
        let generation = self.allocate_generation()?;
        let alt = self
            .alt_setting(interface)
            .with_context(|| format!("USB interface {interface} is not active"))?;
        let BindingState::Configured(binding) = &mut self.binding else {
            anyhow::bail!("cannot select an interface without a configuration binding")
        };
        ensure!(
            binding.interfaces.contains_key(&interface),
            "RDPEUSB configuration has no binding for interface {interface}"
        );
        // TODO: Cancel pending packets and stop streams only for this
        // interface before replacing its pipe handles.
        binding.interfaces.insert(
            interface,
            InterfaceBindingState::Unknown {
                alternate_setting: alt,
                generation,
            },
        );
        Ok(InterfaceSelectionToken {
            interface,
            generation,
        })
    }

    pub(super) fn validate_interface_result(
        &self,
        plan: &ConfigurationPlan,
        interface: u8,
        result: TsUrbSelectInterfaceResult,
        token: InterfaceSelectionToken,
    ) -> anyhow::Result<InterfaceBinding> {
        ensure!(
            token.interface == interface,
            "interface selection token belongs to interface {} instead of {interface}",
            token.interface
        );
        let descriptor = plan
            .descriptor
            .as_ref()
            .context("alternate-setting plan has no configuration descriptor")?;
        let alt = plan.alt_setting(interface)?;
        descriptor.validate_interface_result(interface, alt, plan.usb_speed, &result.interface)?;
        InterfaceBinding::from_result(result.interface, token.generation)
    }

    pub(super) fn commit_alt_setting(
        &mut self,
        plan: ConfigurationPlan,
        interface: u8,
        binding: InterfaceBinding,
        token: InterfaceSelectionToken,
    ) -> anyhow::Result<()> {
        ensure!(
            token.interface == interface,
            "interface selection token belongs to interface {} instead of {interface}",
            token.interface
        );
        ensure!(
            plan.selection.configuration == self.configuration(),
            "alternate-setting plan changed the active configuration"
        );
        ensure!(
            binding.alternate_setting == plan.alt_setting(interface)?,
            "RDPEUSB interface binding does not match alternate-setting plan"
        );
        ensure!(
            binding.generation == token.generation,
            "RDPEUSB interface binding belongs to a stale selection transaction"
        );
        let revision = self
            .announced
            .revision
            .checked_add(1)
            .context("usbredir announcement revision exhausted")?;
        let BindingState::Configured(configuration_binding) = &mut self.binding else {
            anyhow::bail!("configuration binding disappeared during interface selection")
        };
        ensure!(
            matches!(
                configuration_binding.interfaces.get(&interface),
                Some(InterfaceBindingState::Unknown { generation, .. })
                    if *generation == token.generation
            ),
            "interface selection transaction is no longer current"
        );
        configuration_binding
            .interfaces
            .insert(interface, InterfaceBindingState::Bound(binding));
        self.announced = plan.into_announced(revision);
        Ok(())
    }

    fn validate_configuration_commit(
        &self,
        plan: &ConfigurationPlan,
        binding: &BindingState,
        token: ConfigurationSelectionToken,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(
                &self.binding,
                BindingState::Unknown { generation } if *generation == token.generation
            ),
            "configuration selection transaction is no longer current"
        );
        ensure!(
            binding.generation() == token.generation,
            "RDPEUSB configuration binding belongs to a stale selection transaction"
        );
        if let BindingState::Configured(binding) = binding {
            ensure!(
                binding.configuration == plan.selection.configuration,
                "RDPEUSB binding configuration does not match selection plan"
            );
        }
        Ok(())
    }

    fn allocate_generation(&mut self) -> anyhow::Result<u64> {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .context("RDPEUSB binding generation exhausted")?;
        Ok(self.next_generation)
    }
}

impl BindingState {
    fn generation(&self) -> u64 {
        match self {
            Self::Unconfigured { generation } | Self::Unknown { generation } => *generation,
            Self::Configured(binding) => binding.generation,
        }
    }
}

impl ConfigurationPlan {
    fn new(
        device_descriptor: &DeviceDescriptor,
        speed: Speed,
        selection: DeviceSelection,
        descriptor: Option<ConfigurationDescriptor>,
    ) -> anyhow::Result<Self> {
        ensure!(
            (selection.configuration == 0) == descriptor.is_none(),
            "USB configuration zero must be represented without a descriptor"
        );
        if let Some(descriptor) = &descriptor {
            ensure!(
                descriptor.configuration_value() == selection.configuration,
                "USB configuration plan uses the wrong descriptor"
            );
        }
        let (interface_info, ep_info) = descriptor::usbredir_config_info(
            device_descriptor,
            descriptor.as_ref(),
            &selection.active_interfaces,
            speed,
        )?;
        let usb_speed = descriptor::usb_speed(speed)?;
        Ok(Self {
            selection,
            descriptor,
            usb_speed,
            interface_info,
            ep_info,
        })
    }

    pub(super) fn alt_setting(&self, interface: u8) -> anyhow::Result<u8> {
        self.selection
            .active_interfaces
            .iter()
            .find_map(|&(number, alt)| (number == interface).then_some(alt))
            .with_context(|| format!("USB interface {interface} is not in selection plan"))
    }

    pub(super) fn select_configuration_request(&self) -> anyhow::Result<TransferInPacket> {
        let active_interfaces = self
            .selection
            .active_interfaces
            .iter()
            .map(|&(interface, alternate_setting)| UsbInterfaceSelection {
                interface,
                alternate_setting,
            })
            .collect::<Vec<_>>();
        rdpeusb::select_configuration(
            self.descriptor.as_ref().map(ConfigurationDescriptor::view),
            &active_interfaces,
            self.usb_speed,
        )
        .map_err(|error| anyhow::anyhow!("cannot translate USB configuration selection: {error}"))
    }

    pub(super) fn select_interface_request(
        &self,
        config_handle: ConfigHandle,
        interface: u8,
    ) -> anyhow::Result<TransferInPacket> {
        let descriptor = self
            .descriptor
            .as_ref()
            .context("alternate-setting plan has no configuration descriptor")?;
        let alternate_setting = self.alt_setting(interface)?;
        let interface = descriptor
            .view()
            .interface(interface, alternate_setting)
            .with_context(|| {
                format!("USB interface {interface} has no alternate setting {alternate_setting}")
            })?;
        rdpeusb::select_interface(config_handle, interface, self.usb_speed)
            .map_err(|error| anyhow::anyhow!("cannot translate USB interface selection: {error}"))
    }

    fn into_announced(self, revision: u64) -> AnnouncedView {
        AnnouncedView {
            selection: self.selection,
            interface_info: self.interface_info,
            ep_info: self.ep_info,
            revision,
        }
    }
}

impl InterfaceBinding {
    fn from_result(
        result: ironrdp_rdpeusb::pdu::completion::ts_urb_result::TsUsbdInterfaceInfoResult,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut pipes = HashMap::with_capacity(result.pipes.len());
        for pipe in result.pipes {
            let endpoint = Endpoint::from_raw(pipe.endpoint_address);
            ensure!(
                pipes
                    .insert(
                        endpoint,
                        PipeBinding {
                            handle: pipe.pipe_handle,
                            pipe_type: pipe.pipe_type,
                            max_packet_size: pipe.max_packet_size,
                            interval: pipe.interval,
                            max_transfer_size: pipe.max_transfer_size,
                            flags: pipe.pipe_flags,
                        },
                    )
                    .is_none(),
                "RDPEUSB returned duplicate endpoint {}",
                endpoint
            );
        }
        Ok(Self {
            alternate_setting: result.alternate_setting,
            interface_handle: result.interface_handle,
            generation,
            pipes,
        })
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_rdpeusb::{
        io::{DeviceAnnounce, TsUrbInKind},
        pdu::{
            completion::ts_urb_result::{
                TsUrbSelectConfigResult, TsUrbSelectInterfaceResult, TsUsbdInterfaceInfoResult,
                TsUsbdPipeInfoResult, UsbdPipeType,
            },
            sink::{
                DeviceSpeed, NoAckIsochWriteJitterBufSizeInMs, SupportedUsbVer, UsbBusIfaceVer,
                UsbDeviceCaps, UsbdiVer,
            },
        },
    };
    use usbredir_proto::{Endpoint, Speed, TransferType};

    use super::super::{descriptor::ConfigurationDescriptor, device::PreparedDevice};
    use super::ActiveDevice;

    const DEVICE_DESCRIPTOR: [u8; 18] = [
        18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01, 1, 2, 3, 1,
    ];
    const CONFIGURATION: [u8; 48] = [
        9, 2, 48, 0, 1, 2, 7, 0x80, 50, // configuration 2
        9, 4, 3, 0, 2, 0xff, 1, 2, 0, // interface 3, alt 0
        7, 5, 0x01, 2, 64, 0, 0, // bulk OUT endpoint 1
        7, 5, 0x81, 3, 8, 0, 10, // interrupt IN endpoint 1
        9, 4, 3, 1, 1, 0xfe, 3, 4, 0, // interface 3, alt 1
        7, 5, 0x82, 1, 128, 0, 1, // isochronous IN endpoint 2
    ];
    const EMPTY_CONFIGURATION: [u8; 9] = [9, 2, 9, 0, 0, 3, 0, 0x80, 1];

    #[test]
    fn configuration_descriptors_are_cached_once() {
        let mut device = active_device(1);

        assert_eq!(device.device_descriptor_bytes(), DEVICE_DESCRIPTOR);

        assert!(
            ConfigurationDescriptor::parse_if_complete(&CONFIGURATION[..9])
                .unwrap()
                .is_none()
        );
        assert!(!device.has_configuration(2));
        assert!(device.configuration_descriptor_by_index(0).is_none());

        let descriptor = ConfigurationDescriptor::parse(&CONFIGURATION).unwrap();
        device
            .insert_configuration_descriptor(0, descriptor)
            .unwrap();
        assert!(device.has_configuration(2));
        assert_eq!(
            device.configuration_descriptor_by_index(0).unwrap().bytes(),
            CONFIGURATION
        );
        assert!(device
            .insert_configuration_descriptor(
                0,
                ConfigurationDescriptor::parse(&CONFIGURATION).unwrap(),
            )
            .is_err());

        let mut duplicate_value = active_device(2);
        duplicate_value
            .insert_configuration_descriptor(
                0,
                ConfigurationDescriptor::parse(&CONFIGURATION).unwrap(),
            )
            .unwrap();
        assert!(duplicate_value
            .insert_configuration_descriptor(
                1,
                ConfigurationDescriptor::parse(&CONFIGURATION).unwrap(),
            )
            .is_err());

        let mut header_sized_configuration = active_device(1);
        let descriptor = ConfigurationDescriptor::parse_if_complete(&EMPTY_CONFIGURATION)
            .unwrap()
            .unwrap();
        header_sized_configuration
            .insert_configuration_descriptor(0, descriptor)
            .unwrap();
        assert!(header_sized_configuration.has_configuration(3));
    }

    #[test]
    fn logical_unconfigured_device_is_bound_only_by_real_selects() {
        let mut device = active_device(1);
        assert_eq!(device.configuration(), 0);
        assert_eq!(device.alt_setting(3), None);
        assert!(device.configuration_handle().is_err());
        assert!(device.bulk_pipe_handle(Endpoint::from_raw(0x01)).is_err());
        let (ep_info, interface_info) = device.metadata();
        assert_eq!(interface_info.interface_count, 0);
        for (index, kind) in ep_info.ep_type.iter().enumerate() {
            let expected = if matches!(index, 0 | 16) {
                TransferType::Control
            } else {
                TransferType::Invalid
            };
            assert_eq!(*kind, expected);
        }

        device
            .insert_configuration_descriptor(
                0,
                ConfigurationDescriptor::parse(&CONFIGURATION).unwrap(),
            )
            .unwrap();

        let config_plan = device.plan_configuration(2).unwrap();
        let request = config_plan.select_configuration_request().unwrap();
        let TsUrbInKind::SelectConfig(request) = request.ts_urb.kind else {
            panic!("configuration plan did not produce SELECT_CONFIGURATION")
        };
        assert_eq!(request.desc.unwrap().configuration_value, 2);
        let config_result = TsUrbSelectConfigResult {
            config_handle: 0xcccc_0002,
            interface: vec![interface_result(
                0,
                0x1111_0003,
                vec![
                    pipe_result(0x81, UsbdPipeType::Interrupt, 0x8181_0001),
                    pipe_result(0x01, UsbdPipeType::Bulk, 0x0101_0001),
                ],
            )],
        };
        let stale_token = device.begin_configuration_selection().unwrap();
        assert!(device.configuration_handle().is_err());
        assert!(device
            .configuration_handle_for_interface_selection()
            .is_err());
        let stale_binding = device
            .validate_configuration_result(&config_plan, config_result.clone(), stale_token)
            .unwrap();
        let token = device.begin_configuration_selection().unwrap();
        assert!(
            device
                .commit_configuration(config_plan, stale_binding, stale_token)
                .is_err(),
            "a superseded selection transaction must not install stale handles"
        );

        let config_plan = device.plan_configuration(2).unwrap();
        let binding = device
            .validate_configuration_result(&config_plan, config_result, token)
            .unwrap();
        device
            .commit_configuration(config_plan, binding, token)
            .unwrap();
        assert_eq!(device.configuration(), 2);
        assert_eq!(device.configuration_handle().unwrap(), 0xcccc_0002);
        assert_eq!(
            device.bulk_pipe_handle(Endpoint::from_raw(0x01)).unwrap(),
            0x0101_0001
        );
        assert!(device.bulk_pipe_handle(Endpoint::from_raw(0x81)).is_err());
        assert!(device.bulk_pipe_handle(Endpoint::from_raw(0x02)).is_err());

        let alt_plan = device.plan_alt_setting(3, 1).unwrap();
        let request = alt_plan
            .select_interface_request(device.configuration_handle().unwrap(), 3)
            .unwrap();
        let TsUrbInKind::SelectIface(request) = request.ts_urb.kind else {
            panic!("alternate-setting plan did not produce SELECT_INTERFACE")
        };
        assert_eq!(request.config_handle, 0xcccc_0002);
        assert_eq!(request.usbd_iface.alternate_setting, 1);
        let _abandoned_token = device.begin_interface_selection(3).unwrap();
        assert!(device.configuration_handle().is_err());
        assert_eq!(
            device
                .configuration_handle_for_interface_selection()
                .unwrap(),
            0xcccc_0002
        );
        let token = device.begin_interface_selection(3).unwrap();
        let binding = device
            .validate_interface_result(
                &alt_plan,
                3,
                TsUrbSelectInterfaceResult {
                    interface: TsUsbdInterfaceInfoResult {
                        interface_number: 3,
                        alternate_setting: 1,
                        class: 0xfe,
                        sub_class: 3,
                        protocol: 4,
                        interface_handle: 0x2222_0003,
                        pipes: vec![TsUsbdPipeInfoResult {
                            max_packet_size: 128,
                            endpoint_address: 0x82,
                            interval: 1,
                            pipe_type: UsbdPipeType::Isochronous,
                            pipe_handle: 0x8282_0002,
                            max_transfer_size: 4096,
                            pipe_flags: 0,
                        }],
                    },
                },
                token,
            )
            .unwrap();
        device
            .commit_alt_setting(alt_plan, 3, binding, token)
            .unwrap();
        assert_eq!(device.alt_setting(3), Some(1));
        assert_eq!(device.configuration_handle().unwrap(), 0xcccc_0002);
        assert!(device.bulk_pipe_handle(Endpoint::from_raw(0x01)).is_err());

        let unconfigure = device.plan_configuration(0).unwrap();
        let request = unconfigure.select_configuration_request().unwrap();
        let TsUrbInKind::SelectConfig(request) = request.ts_urb.kind else {
            panic!("unconfigure plan did not produce SELECT_CONFIGURATION")
        };
        assert!(request.desc.is_none());
        assert!(request.usbd_ifaces.is_empty());
        let token = device.begin_configuration_selection().unwrap();
        let binding = device
            .validate_configuration_result(
                &unconfigure,
                TsUrbSelectConfigResult {
                    config_handle: 0,
                    interface: Vec::new(),
                },
                token,
            )
            .unwrap();
        device
            .commit_configuration(unconfigure, binding, token)
            .unwrap();
        assert_eq!(device.configuration(), 0);
        assert!(device.configuration_handle().is_err());
        assert!(device.bulk_pipe_handle(Endpoint::from_raw(0x01)).is_err());
        let (ep_info, interface_info) = device.metadata();
        assert_eq!(interface_info.interface_count, 0);
        for (index, kind) in ep_info.ep_type.iter().enumerate() {
            let expected = if matches!(index, 0 | 16) {
                TransferType::Control
            } else {
                TransferType::Invalid
            };
            assert_eq!(*kind, expected);
        }
    }

    fn active_device(num_configurations: u8) -> ActiveDevice {
        let mut device_descriptor_bytes = DEVICE_DESCRIPTOR;
        device_descriptor_bytes[17] = num_configurations;
        ActiveDevice::from_prepared(PreparedDevice {
            source: DeviceAnnounce {
                device_instance_id: "test-device".into(),
                hw_ids: Vec::new(),
                compat_ids: Vec::new(),
                container_id: String::new(),
                usb_device_caps: UsbDeviceCaps {
                    usb_bus_iface_ver: UsbBusIfaceVer::V1,
                    usbdi_ver: UsbdiVer::V0X600,
                    supported_usb_ver: SupportedUsbVer::USB_20,
                    device_speed: DeviceSpeed::HIGH_SPEED,
                    no_ack_isoch_write_jitter_buf_size: NoAckIsochWriteJitterBufSizeInMs::try_from(
                        0,
                    )
                    .unwrap(),
                },
            },
            device_descriptor_bytes: device_descriptor_bytes.to_vec(),
            speed: Speed::High,
        })
        .unwrap()
    }

    fn interface_result(
        alternate_setting: u8,
        interface_handle: u32,
        pipes: Vec<TsUsbdPipeInfoResult>,
    ) -> TsUsbdInterfaceInfoResult {
        TsUsbdInterfaceInfoResult {
            interface_number: 3,
            alternate_setting,
            class: 0xff,
            sub_class: 1,
            protocol: 2,
            interface_handle,
            pipes,
        }
    }

    fn pipe_result(
        endpoint_address: u8,
        pipe_type: UsbdPipeType,
        pipe_handle: u32,
    ) -> TsUsbdPipeInfoResult {
        let (max_packet_size, interval) = match endpoint_address {
            0x01 => (64, 0),
            0x81 => (8, 10),
            _ => unreachable!("unexpected test endpoint"),
        };
        TsUsbdPipeInfoResult {
            max_packet_size,
            endpoint_address,
            interval,
            pipe_type,
            pipe_handle,
            max_transfer_size: 65_536,
            pipe_flags: 0,
        }
    }
}
