//! x86-64 specific transports.

use super::{
    pci::{
        bus::{Cam, ConfigurationAccess, DeviceFunction, PciRoot, PCI_CAP_ID_VNDR},
        device_type, CommonCfg, VirtioCapabilityInfo, VirtioPciError, CAP_BAR_OFFSET,
        CAP_BAR_OFFSET_OFFSET, CAP_LENGTH_OFFSET, CAP_NOTIFY_OFF_MULTIPLIER_OFFSET,
        VIRTIO_PCI_CAP_COMMON_CFG, VIRTIO_PCI_CAP_DEVICE_CFG, VIRTIO_PCI_CAP_ISR_CFG,
        VIRTIO_PCI_CAP_NOTIFY_CFG, VIRTIO_VENDOR_ID,
    },
    DeviceStatus, DeviceType, Transport,
};
use crate::{
    hal::{Hal, PhysAddr},
    Error,
};
use core::arch::asm;
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// This CPUID returns the signature and should be used to determine if VM is running under pKVM,
/// KVM or not. See the Linux header `arch/x86/include/uapi/asm/kvm_para.h`.
const KVM_CPUID_SIGNATURE: u32 = 0x40000000;

// See `include/uapi/linux/kvm_para.h`. (These hypercalls numbers can change depending on the
// upstream progress.)
const KVM_HC_PKVM_OP: u32 = 20;
const PKVM_GHC_IOREAD: u32 = KVM_HC_PKVM_OP + 3;
const PKVM_GHC_IOWRITE: u32 = KVM_HC_PKVM_OP + 4;

const PKVM_SIGNATURE: &[u8] = b"PKVM";

/// The maximum number of bytes that can be read or written by a single IO hypercall.
const HYP_IO_MAX: usize = 8;

/// A PCI configuration access mechanism using hypercalls implemented by the x86-64 pKVM hypervisor.
pub struct HypCam {
    /// The physical base address of the PCI root complex.
    phys_base: usize,
    cam: Cam,
}

impl HypCam {
    /// Creates a new `HypCam` for the PCI root complex at the given physical base address.
    pub fn new(phys_base: usize, cam: Cam) -> Self {
        Self { phys_base, cam }
    }

    /// Returns whether we are running under pKVM by checking the CPU ID signature.
    pub fn is_pkvm() -> bool {
        cpuid_signature() == PKVM_SIGNATURE
    }
}

impl ConfigurationAccess for HypCam {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        let address = self.cam.cam_offset(device_function, register_offset);
        hyp_io_read(self.phys_base + (address as usize), 4) as u32
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        let address = self.cam.cam_offset(device_function, register_offset);
        hyp_io_write(self.phys_base + (address as usize), 4, data.into());
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            phys_base: self.phys_base,
            cam: self.cam,
        }
    }
}

macro_rules! configread {
    ($common_cfg:expr, $field:ident) => {
        $common_cfg.read(core::mem::offset_of!(CommonCfg, $field))
    };
}

macro_rules! configwrite {
    ($common_cfg:expr, $field:ident, $value:expr) => {
        $common_cfg.write(core::mem::offset_of!(CommonCfg, $field), $value)
    };
}

/// PCI transport for VirtIO using hypercalls implemented by the x86-64 pKVM hypervisor for IO BARs.
#[derive(Debug)]
pub struct HypPciTransport {
    device_type: DeviceType,
    /// The bus, device and function identifier for the VirtIO device.
    device_function: DeviceFunction,
    /// The common configuration structure within some BAR.
    common_cfg: HypIoRegion,
    /// The start of the queue notification region within some BAR.
    notify_region: HypIoRegion,
    notify_off_multiplier: u32,
    /// The ISR status register within some BAR.
    isr_status: HypIoRegion,
    /// The VirtIO device-specific configuration within some BAR.
    config_space: Option<HypIoRegion>,
}

impl HypPciTransport {
    /// Constructs a new x86-64 pKVM PCI VirtIO transport for the given device function on the given
    /// PCI root controller.
    pub fn new<H: Hal, C: ConfigurationAccess>(
        root: &mut PciRoot<C>,
        device_function: DeviceFunction,
    ) -> Result<Self, VirtioPciError> {
        let device_vendor = root.configuration_access.read_word(device_function, 0);
        let device_id = (device_vendor >> 16) as u16;
        let vendor_id = device_vendor as u16;
        if vendor_id != VIRTIO_VENDOR_ID {
            return Err(VirtioPciError::InvalidVendorId(vendor_id));
        }
        let device_type = device_type(device_id);

        // Find the PCI capabilities we need.
        let mut common_cfg = None;
        let mut notify_cfg = None;
        let mut notify_off_multiplier = 0;
        let mut isr_cfg = None;
        let mut device_cfg = None;
        for capability in root.capabilities(device_function) {
            if capability.id != PCI_CAP_ID_VNDR {
                continue;
            }
            let cap_len = capability.private_header as u8;
            let cfg_type = (capability.private_header >> 8) as u8;
            if cap_len < 16 {
                continue;
            }
            let struct_info = VirtioCapabilityInfo {
                bar: root
                    .configuration_access
                    .read_word(device_function, capability.offset + CAP_BAR_OFFSET)
                    as u8,
                offset: root
                    .configuration_access
                    .read_word(device_function, capability.offset + CAP_BAR_OFFSET_OFFSET),
                length: root
                    .configuration_access
                    .read_word(device_function, capability.offset + CAP_LENGTH_OFFSET),
            };

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common_cfg.is_none() => {
                    common_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_NOTIFY_CFG if cap_len >= 20 && notify_cfg.is_none() => {
                    notify_cfg = Some(struct_info);
                    notify_off_multiplier = root.configuration_access.read_word(
                        device_function,
                        capability.offset + CAP_NOTIFY_OFF_MULTIPLIER_OFFSET,
                    );
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr_cfg.is_none() => {
                    isr_cfg = Some(struct_info);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG if device_cfg.is_none() => {
                    device_cfg = Some(struct_info);
                }
                _ => {}
            }
        }

        let common_cfg = get_bar_region::<H, CommonCfg, _>(
            root,
            device_function,
            &common_cfg.ok_or(VirtioPciError::MissingCommonConfig)?,
        )?;

        let notify_cfg = notify_cfg.ok_or(VirtioPciError::MissingNotifyConfig)?;
        if notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                notify_off_multiplier,
            ));
        }
        let notify_region = get_bar_region::<H, u16, _>(root, device_function, &notify_cfg)?;

        let isr_status = get_bar_region::<H, u8, _>(
            root,
            device_function,
            &isr_cfg.ok_or(VirtioPciError::MissingIsrConfig)?,
        )?;

        let config_space = if let Some(device_cfg) = device_cfg {
            Some(get_bar_region::<H, u32, _>(
                root,
                device_function,
                &device_cfg,
            )?)
        } else {
            None
        };

        Ok(Self {
            device_type,
            device_function,
            common_cfg,
            notify_region,
            notify_off_multiplier,
            isr_status,
            config_space,
        })
    }
}

impl Transport for HypPciTransport {
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn read_device_features(&mut self) -> u64 {
        configwrite!(self.common_cfg, device_feature_select, 0u32);
        let device_features_low: u32 = configread!(self.common_cfg, device_feature);
        configwrite!(self.common_cfg, device_feature_select, 1u32);
        let device_features_high: u32 = configread!(self.common_cfg, device_feature);
        (device_features_high as u64) << 32 | device_features_low as u64
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        configwrite!(self.common_cfg, driver_feature_select, 0u32);
        configwrite!(self.common_cfg, driver_feature, driver_features as u32);
        configwrite!(self.common_cfg, driver_feature_select, 1u32);
        configwrite!(
            self.common_cfg,
            driver_feature,
            (driver_features >> 32) as u32
        );
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        configwrite!(self.common_cfg, queue_select, queue);
        let queue_size: u16 = configread!(self.common_cfg, queue_size);
        queue_size.into()
    }

    fn notify(&mut self, queue: u16) {
        configwrite!(self.common_cfg, queue_select, queue);
        // TODO: Consider caching this somewhere (per queue).
        let queue_notify_off: u16 = configread!(self.common_cfg, queue_notify_off);

        let offset_bytes = usize::from(queue_notify_off) * self.notify_off_multiplier as usize;
        self.notify_region.write(offset_bytes, queue);
    }

    fn get_status(&self) -> DeviceStatus {
        let status: u8 = configread!(self.common_cfg, device_status);
        DeviceStatus::from_bits_truncate(status.into())
    }

    fn set_status(&mut self, status: DeviceStatus) {
        configwrite!(self.common_cfg, device_status, status.bits() as u8);
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {
        // No-op, the PCI transport doesn't care.
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        configwrite!(self.common_cfg, queue_select, queue);
        configwrite!(self.common_cfg, queue_size, size as u16);
        configwrite!(self.common_cfg, queue_desc, descriptors as u64);
        configwrite!(self.common_cfg, queue_driver, driver_area as u64);
        configwrite!(self.common_cfg, queue_device, device_area as u64);
        configwrite!(self.common_cfg, queue_enable, 1u16);
    }

    fn queue_unset(&mut self, _queue: u16) {
        // The VirtIO spec doesn't allow queues to be unset once they have been set up for the PCI
        // transport, so this is a no-op.
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        configwrite!(self.common_cfg, queue_select, queue);
        let queue_enable: u16 = configread!(self.common_cfg, queue_enable);
        queue_enable == 1
    }

    fn ack_interrupt(&mut self) -> bool {
        // Safe because the common config pointer is valid and we checked in get_bar_region that it
        // was aligned.
        // Reading the ISR status resets it to 0 and causes the device to de-assert the interrupt.
        let isr_status: u8 = self.isr_status.read(0);
        // TODO: Distinguish between queue interrupt and device configuration interrupt.
        isr_status & 0x3 != 0
    }

    fn read_config_generation(&self) -> u32 {
        configread!(self.common_cfg, config_generation)
    }

    fn read_config_space<T: FromBytes>(&self, offset: usize) -> Result<T, Error> {
        assert!(align_of::<T>() <= 4,
            "Driver expected config space alignment of {} bytes, but VirtIO only guarantees 4 byte alignment.",
            align_of::<T>());
        assert_eq!(offset % align_of::<T>(), 0);

        let config_space = self.config_space.ok_or(Error::ConfigSpaceMissing)?;
        if config_space.size < offset + size_of::<T>() {
            Err(Error::ConfigSpaceTooSmall)
        } else {
            Ok(config_space.read(offset))
        }
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<(), Error> {
        assert!(align_of::<T>() <= 4,
            "Driver expected config space alignment of {} bytes, but VirtIO only guarantees 4 byte alignment.",
            align_of::<T>());
        assert_eq!(offset % align_of::<T>(), 0);

        let config_space = self.config_space.ok_or(Error::ConfigSpaceMissing)?;
        if config_space.size < offset + size_of::<T>() {
            Err(Error::ConfigSpaceTooSmall)
        } else {
            config_space.write(offset, value);
            Ok(())
        }
    }
}

fn get_bar_region<H: Hal, T, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    device_function: DeviceFunction,
    struct_info: &VirtioCapabilityInfo,
) -> Result<HypIoRegion, VirtioPciError> {
    let bar_info = root.bar_info(device_function, struct_info.bar)?;
    let (bar_address, bar_size) = bar_info
        .memory_address_size()
        .ok_or(VirtioPciError::UnexpectedIoBar)?;
    if bar_address == 0 {
        return Err(VirtioPciError::BarNotAllocated(struct_info.bar));
    }
    if struct_info.offset + struct_info.length > bar_size
        || size_of::<T>() > struct_info.length as usize
    {
        return Err(VirtioPciError::BarOffsetOutOfRange);
    }
    let paddr = bar_address as PhysAddr + struct_info.offset as PhysAddr;
    if paddr % align_of::<T>() != 0 {
        return Err(VirtioPciError::Misaligned {
            address: paddr,
            alignment: align_of::<T>(),
        });
    }
    Ok(HypIoRegion {
        paddr,
        size: struct_info.length as usize,
    })
}

/// Gets the signature CPU ID.
fn cpuid_signature() -> [u8; 4] {
    let signature: u32;
    unsafe {
        // The argument for cpuid is passed via rax and in case of KVM_CPUID_SIGNATURE returned via
        // rbx, rcx and rdx. Ideally using a named argument in inline asm for rbx would be more
        // straightforward, but when "rbx" is directly used LLVM complains that it is used
        // internally.
        //
        // Therefore use r8 instead and push rbx to the stack before making cpuid call, store
        // rbx content to r8 as use it as inline asm output and pop the rbx.
        asm!(
            "push rbx",
            "cpuid",
            "mov r8, rbx",
            "pop rbx",
            in("eax") KVM_CPUID_SIGNATURE,
            out("r8") signature,
            out("rcx") _,
            out("rdx") _,
        );
    };
    signature.to_le_bytes()
}

/// Asks the hypervisor to perform an IO read at the given physical address.
fn hyp_io_read(address: usize, size: usize) -> u64 {
    // Arguments for vmcall are passed via rax, rbx, rcx and rdx. Ideally using a named argument in
    // the inline asm for rbx would be more straightforward, but when "rbx" is used directly LLVM
    // complains that it is used internally.
    //
    // Therefore use r8 temporary, push rbx to the stack, perform proper call and pop rbx
    // again
    let data;
    unsafe {
        asm!(
            "push rbx",
            "mov rbx, r8",
            "vmcall",
            "pop rbx",
            inout("rax") u64::from(PKVM_GHC_IOREAD) => data,
            in("r8") address,
            in("rcx") size,
        );
    }
    data
}

/// Asks the hypervisor to perform an IO write at the given physical address.
fn hyp_io_write(address: usize, size: usize, data: u64) {
    unsafe {
        // Arguments for vmcall are passed via rax, rbx, rcx and rdx. Ideally using a named argument
        // in the inline asm for rbx would be more straightforward but when "rbx" is used directly
        // used LLVM complains that it is used internally.
        //
        // Therefore use r8 temporary, push rbx to the stack, perform proper call and pop rbx
        // again
        asm!(
            "push rbx",
            "mov rbx, r8",
            "vmcall",
            "pop rbx",
            in("rax") PKVM_GHC_IOWRITE,
            in("r8") address,
            in("rcx") size,
            in("rdx") data,
        );
    }
}

/// A region of physical address space which may be accessed by IO read and/or write hypercalls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HypIoRegion {
    /// The physical address of the start of the IO region.
    paddr: usize,
    /// The size of the IO region in bytes.
    size: usize,
}

impl HypIoRegion {
    fn read<T: FromBytes>(self, offset: usize) -> T {
        assert!(offset + size_of::<T>() <= self.size);
        assert!(size_of::<T>() < HYP_IO_MAX);

        let data = hyp_io_read(self.paddr + offset, size_of::<T>());
        T::read_from_prefix(data.as_bytes()).unwrap().0
    }

    fn write<T: IntoBytes + Immutable>(self, offset: usize, value: T) {
        assert!(offset + size_of::<T>() <= self.size);
        assert!(size_of::<T>() < HYP_IO_MAX);

        let mut data = 0;
        data.as_mut_bytes()[..size_of::<T>()].copy_from_slice(value.as_bytes());
        hyp_io_write(self.paddr + offset, size_of::<T>(), data);
    }
}
