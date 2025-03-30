use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::convert::Into;
use core::marker::PhantomData;
use core::option::Option;
use core::sync::atomic::{AtomicBool, Ordering};
use memory_addr::{PhysAddr, VirtAddr};
use page_table_multiarch::{PageSize, PagingHandler, PagingResult};

use axerrno::{AxResult, ax_err, ax_err_type};
use spin::Mutex;

use axaddrspace::{AddrSpace, GuestPhysAddr, HostPhysAddr, MappingFlags};
use axdevice::{AxVmDeviceConfig, AxVmDevices};
use axvcpu::{AxArchVCpu, AxVCpu, AxVCpuExitReason, AxVCpuHal};

use crate::config::{AxVMConfig, VmMemMappingType};
use crate::vcpu::{AxArchVCpuImpl, AxVCpuCreateConfig};
use crate::{AxVMHal, has_hardware_support};

#[cfg(feature = "gdb")]
use crate::config::GdbConnection;
#[cfg(feature = "gdb")]
use crate::gdb::GdbTarget;
#[cfg(feature = "gdb")]
use gdbstub::stub::state_machine::GdbStubStateMachine;

const VM_ASPACE_BASE: usize = 0x0;
const VM_ASPACE_SIZE: usize = 0x7fff_ffff_f000;

/// Type alias for a vCPU with architecture-independent interface.
#[allow(type_alias_bounds)]
type VCpu<U: AxVCpuHal> = AxVCpu<AxArchVCpuImpl<U>>;
/// Type alias for a reference to a vCPU.
#[allow(type_alias_bounds)]
pub type AxVCpuRef<U: AxVCpuHal> = Arc<VCpu<U>>;
/// Type alias for a reference to a VM.
#[allow(type_alias_bounds)]
pub type AxVMRef<H: AxVMHal, U: AxVCpuHal> = Arc<AxVM<H, U>>; // we know the bound is not enforced here, we keep it for clarity

use axaddrspace::npt::NestedPageTable as PageTable;

pub struct PagingHandlerEpt<F>(Option<F>);

impl<F> PagingHandlerEpt<F>
where
    F: Fn(PhysAddr) -> VirtAddr,
{
    pub fn set_callback(&mut self, closure: F) {
        self.0 = Some(closure)
    }
}

impl<F> PagingHandler for PagingHandlerEpt<F>
where
    F: Fn(PhysAddr) -> VirtAddr,
{
    fn new() -> Self {
        Self(None)
    }
    fn alloc_frame(&self) -> Option<PhysAddr> {
        None
    }

    fn dealloc_frame(&self, _: PhysAddr) {}

    #[inline]
    fn phys_to_virt(&self, paddr: PhysAddr) -> VirtAddr {
        self.0.as_ref().map(|f| f(paddr)).unwrap()
    }
}

struct AxVMInnerConst<U: AxVCpuHal> {
    id: usize,
    config: AxVMConfig,
    vcpu_list: Box<[AxVCpuRef<U>]>,
    devices: AxVmDevices,
}

unsafe impl<U: AxVCpuHal> Send for AxVMInnerConst<U> {}
unsafe impl<U: AxVCpuHal> Sync for AxVMInnerConst<U> {}

pub(crate) struct AxVMInnerMut<H: AxVMHal, U: AxVCpuHal> {
    // Todo: use more efficient lock.
    address_space: Mutex<AddrSpace<H::PagingHandler>>,
    #[cfg(feature = "gdb")]
    pub(crate) gdb_target: Mutex<Option<GdbTarget<H, U>>>,
    #[cfg(feature = "gdb")]
    pub(crate) gdb_state:
        Mutex<Option<GdbStubStateMachine<'static, GdbTarget<H, U>, GdbConnection>>>,
    #[cfg(feature = "gdb")]
    pub(crate) saved_inst: Mutex<Option<(u64, [u8; 4], usize)>>,
    _marker: core::marker::PhantomData<(H, U)>,
}

impl<H: AxVMHal, U: AxVCpuHal> AxVMInnerMut<H, U> {
    fn new(address_space: AddrSpace<H::PagingHandler>) -> Self {
        Self {
            address_space: Mutex::new(address_space),
            #[cfg(feature = "gdb")]
            gdb_target: Mutex::new(None),
            #[cfg(feature = "gdb")]
            gdb_state: Mutex::new(None),
            #[cfg(feature = "gdb")]
            saved_inst: Mutex::new(None),
            _marker: PhantomData,
        }
    }
}

unsafe impl<H: AxVMHal, U: AxVCpuHal> Send for AxVMInnerMut<H, U> {}
unsafe impl<H: AxVMHal, U: AxVCpuHal> Sync for AxVMInnerMut<H, U> {}

/// A Virtual Machine.
pub struct AxVM<H: AxVMHal, U: AxVCpuHal> {
    running: AtomicBool,
    inner_const: AxVMInnerConst<U>,
    pub(crate) inner_mut: AxVMInnerMut<H, U>,
}

impl<H: AxVMHal, U: AxVCpuHal> AxVM<H, U> {
    /// Creates a new VM with the given configuration.
    /// Returns an error if the configuration is invalid.
    /// The VM is not started until `boot` is called.
    pub fn new(config: AxVMConfig) -> AxResult<AxVMRef<H, U>> {
        let result = Arc::new({
            let vcpu_id_pcpu_sets = config.get_vcpu_affinities_pcpu_ids();

            // Create VCpus.
            let mut vcpu_list = Vec::with_capacity(vcpu_id_pcpu_sets.len());

            for (vcpu_id, phys_cpu_set, _pcpu_id) in vcpu_id_pcpu_sets {
                #[cfg(target_arch = "aarch64")]
                let arch_config = AxVCpuCreateConfig {
                    mpidr_el1: _pcpu_id as _,
                    dtb_addr: config
                        .image_config()
                        .dtb_load_gpa
                        .unwrap_or_default()
                        .as_usize(),
                };
                #[cfg(target_arch = "riscv64")]
                let arch_config = AxVCpuCreateConfig {
                    hart_id: vcpu_id as _,
                    dtb_addr: config
                        .image_config()
                        .dtb_load_gpa
                        .unwrap_or(GuestPhysAddr::from_usize(0x9000_0000)),
                };
                #[cfg(target_arch = "x86_64")]
                let arch_config = AxVCpuCreateConfig::default();

                vcpu_list.push(Arc::new(VCpu::new(
                    vcpu_id,
                    0, // Currently not used.
                    phys_cpu_set,
                    arch_config,
                )?));
            }

            // Set up Memory regions.
            let mut address_space =
                AddrSpace::new_empty(GuestPhysAddr::from(VM_ASPACE_BASE), VM_ASPACE_SIZE)?;
            for mem_region in config.memory_regions() {
                let mapping_flags = MappingFlags::from_bits(mem_region.flags).ok_or_else(|| {
                    ax_err_type!(
                        InvalidInput,
                        format!("Illegal flags {:?}", mem_region.flags)
                    )
                })?;

                // Check mapping flags.
                if mapping_flags.contains(MappingFlags::DEVICE) {
                    warn!(
                        "Do not include DEVICE flag in memory region flags, it should be configured in pass_through_devices"
                    );
                    continue;
                }

                info!(
                    "Setting up memory region: [{:#x}~{:#x}] {:?}",
                    mem_region.gpa,
                    mem_region.gpa + mem_region.size,
                    mapping_flags
                );

                // Handle ram region.
                match mem_region.map_type {
                    VmMemMappingType::MapIentical => {
                        if H::alloc_memory_region_at(
                            HostPhysAddr::from(mem_region.gpa),
                            mem_region.size,
                        ) {
                            address_space.map_linear(
                                GuestPhysAddr::from(mem_region.gpa),
                                HostPhysAddr::from(mem_region.gpa),
                                mem_region.size,
                                mapping_flags,
                            )?;
                        } else {
                            warn!(
                                "Failed to allocate memory region at {:#x} for VM [{}]",
                                mem_region.gpa,
                                config.id()
                            );
                        }
                    }
                    VmMemMappingType::MapAlloc => {
                        // Note: currently we use `map_alloc`,
                        // which allocates real physical memory in units of physical page frames,
                        // which may not be contiguous!!!
                        address_space.map_alloc(
                            GuestPhysAddr::from(mem_region.gpa),
                            mem_region.size,
                            mapping_flags,
                            true,
                        )?;
                    }
                }
            }

            for pt_device in config.pass_through_devices() {
                info!(
                    "Setting up passthrough device memory region: [{:#x}~{:#x}] -> [{:#x}~{:#x}]",
                    pt_device.base_gpa,
                    pt_device.base_gpa + pt_device.length,
                    pt_device.base_hpa,
                    pt_device.base_hpa + pt_device.length
                );

                address_space.map_linear(
                    GuestPhysAddr::from(pt_device.base_gpa),
                    HostPhysAddr::from(pt_device.base_hpa),
                    pt_device.length,
                    MappingFlags::DEVICE | MappingFlags::READ | MappingFlags::WRITE,
                )?;
            }

            let devices = axdevice::AxVmDevices::new(AxVmDeviceConfig {
                emu_configs: config.emu_devices().to_vec(),
            });

            Self {
                running: AtomicBool::new(false),
                inner_const: AxVMInnerConst {
                    id: config.id(),
                    config,
                    vcpu_list: vcpu_list.into_boxed_slice(),
                    devices,
                },
                inner_mut: AxVMInnerMut::new(address_space),
            }
        });

        info!("VM created: id={}", result.id());

        // Setup VCpus.
        for vcpu in result.vcpu_list() {
            let entry = if vcpu.id() == 0 {
                result.inner_const.config.bsp_entry()
            } else {
                result.inner_const.config.ap_entry()
            };
            vcpu.setup(
                entry,
                result.ept_root(),
                <AxArchVCpuImpl<U> as AxArchVCpu>::SetupConfig::default(),
            )?;
        }
        info!("VM setup: id={}", result.id());

        Ok(result)
    }

    /// Returns the VM id.
    #[inline]
    pub const fn id(&self) -> usize {
        self.inner_const.id
    }

    /// Retrieves the vCPU corresponding to the given vcpu_id for the VM.
    /// Returns None if the vCPU does not exist.
    #[inline]
    pub fn vcpu(&self, vcpu_id: usize) -> Option<AxVCpuRef<U>> {
        self.vcpu_list().get(vcpu_id).cloned()
    }

    /// Returns the number of vCPUs corresponding to the VM.
    #[inline]
    pub const fn vcpu_num(&self) -> usize {
        self.inner_const.vcpu_list.len()
    }

    /// Returns a reference to the list of vCPUs corresponding to the VM.
    #[inline]
    pub fn vcpu_list(&self) -> &[AxVCpuRef<U>] {
        &self.inner_const.vcpu_list
    }

    /// Returns the base address of the two-stage address translation page table for the VM.
    pub fn ept_root(&self) -> HostPhysAddr {
        self.inner_mut.address_space.lock().page_table_root()
    }

    /// Returns guest VM image load region in `Vec<&'static mut [u8]>`,
    /// according to the given `image_load_gpa` and `image_size.
    /// `Vec<&'static mut [u8]>` is a series of (HVA) address segments,
    /// which may correspond to non-contiguous physical addresses,
    ///
    /// FIXME:
    /// Find a more elegant way to manage potentially non-contiguous physical memory
    ///         instead of `Vec<&'static mut [u8]>`.
    pub fn get_image_load_region(
        &self,
        image_load_gpa: GuestPhysAddr,
        image_size: usize,
    ) -> AxResult<Vec<&'static mut [u8]>> {
        let addr_space = self.inner_mut.address_space.lock();
        let image_load_hva = addr_space
            .translated_byte_buffer(image_load_gpa, image_size)
            .expect("Failed to translate kernel image load address");
        Ok(image_load_hva)
    }

    /// Returns if the VM is running.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Boots the VM by setting the running flag as true.
    pub fn boot(&self) -> AxResult {
        if !has_hardware_support() {
            ax_err!(Unsupported, "Hardware does not support virtualization")
        } else if self.running() {
            ax_err!(BadState, format!("VM[{}] is running", self.id()))
        } else {
            info!("Booting VM[{}]", self.id());
            self.running.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    /// Returns this VM's emulated devices.
    pub fn get_devices(&self) -> &AxVmDevices {
        &self.inner_const.devices
    }

    /// Run a vCPU according to the given vcpu_id.
    ///
    /// ## Arguments
    /// * `vcpu_id` - the id of the vCPU to run.
    ///
    /// ## Returns
    /// * `AxVCpuExitReason` - the exit reason of the vCPU, wrapped in an `AxResult`.
    ///
    pub fn run_vcpu(&self, vcpu_id: usize) -> AxResult<AxVCpuExitReason> {
        let vcpu = self
            .vcpu(vcpu_id)
            .ok_or_else(|| ax_err_type!(InvalidInput, "Invalid vcpu_id"))?;

        vcpu.bind()?;

        let exit_reason = loop {
            let exit_reason = vcpu.run()?;
            trace!("{exit_reason:#x?}");
            let handled = match &exit_reason {
                AxVCpuExitReason::MmioRead {
                    addr,
                    width,
                    reg,
                    reg_width: _,
                } => {
                    let val = self
                        .get_devices()
                        .handle_mmio_read(*addr, (*width).into())?;
                    vcpu.set_gpr(*reg, val);
                    true
                }
                AxVCpuExitReason::MmioWrite { addr, width, data } => {
                    self.get_devices()
                        .handle_mmio_write(*addr, (*width).into(), *data as usize);
                    true
                }
                AxVCpuExitReason::IoRead { port: _, width: _ } => true,
                AxVCpuExitReason::IoWrite {
                    port: _,
                    width: _,
                    data: _,
                } => true,
                AxVCpuExitReason::NestedPageFault { addr, access_flags } => self
                    .inner_mut
                    .address_space
                    .lock()
                    .handle_page_fault(*addr, *access_flags),
                _ => false,
            };
            if !handled {
                break exit_reason;
            }
        };

        vcpu.unbind()?;
        Ok(exit_reason)
    }

    /// Read bytes from guest physical memory.
    ///
    /// # Arguments
    /// * `gpa` - Guest physical address to read from
    /// * `size` - Number of bytes to read
    ///
    /// # Returns
    /// * `Option<Vec<u8>>` - The read bytes if successful, None if the address is invalid
    pub fn read_guest_memory(&self, gpa: usize, size: usize) -> Option<Vec<u8>> {
        let addr_space = self.inner_mut.address_space.lock();
        let buffer = addr_space.translated_byte_buffer(GuestPhysAddr::from_usize(gpa), size)?;
        let mut data = vec![0; size];
        for (i, slice) in buffer.iter().enumerate() {
            data[i..i + slice.len()].copy_from_slice(slice);
        }
        Some(data)
    }

    /// Write bytes to guest physical memory.
    ///
    /// # Arguments
    /// * `gpa` - Guest physical address to write to
    /// * `data` - Bytes to write
    ///
    /// # Returns
    /// * `Option<()>` - Some(()) if successful, None if the address is invalid
    pub fn write_guest_memory(&self, gpa: usize, data: &[u8]) -> Option<()> {
        let addr_space = self.inner_mut.address_space.lock();
        let mut buffer =
            addr_space.translated_byte_buffer(GuestPhysAddr::from_usize(gpa), data.len())?;
        for (i, slice) in buffer.iter_mut().enumerate() {
            slice.copy_from_slice(&data[i..i + slice.len()]);
        }
        Some(())
    }

    pub fn get_page(
        &self,
        page_table: GuestPhysAddr,
        addr: u64,
    ) -> PagingResult<(PhysAddr, MappingFlags, PageSize)> {
        let addr_space = self.inner_mut.address_space.lock();
        let _vcpu = self.vcpu(0).unwrap();
        // Handle Result from get_ept_root() using unwrap_or_else
        let root_paddr = page_table.as_usize();
        let mut paging = PagingHandlerEpt::new();
        paging.set_callback(|guest_addr| {
            // Convert PhysAddr to GuestPhysAddr then translate
            let gpa = GuestPhysAddr::from(guest_addr.as_usize());
            let host_addr = addr_space.translate(gpa).unwrap();
            // Convert HostPhysAddr to VirtAddr through HAL
            H::phys_to_virt(host_addr)
        });
        PageTable::create_from(root_paddr.into(), paging).query((addr as usize).into())
    }

    /// Read bytes from guest virtual memory.
    ///
    /// # Arguments
    /// * `gva` - Guest virtual address to read from
    /// * `size` - Number of bytes to read
    ///
    /// # Returns
    /// * `Option<Vec<u8>>` - The read bytes if successful, None if the address is invalid
    pub fn read_guest_virtual_memory(&self, vcpu_id: usize,  gva: usize, len: usize) -> Option<Vec<u8>> {
        if let Some(vcpu) = self.vcpu(vcpu_id) {
            let (mut addr, mut count) = (gva, len);
            let page_table = vcpu.get_page_table_root();
            if page_table != 0.into() {
                let (paddr, _, size) = self
                    .get_page(page_table, addr as u64).ok()?;
                addr = paddr.as_usize();
                count = count.min(size as usize);
            }
            // Return error when unwrap is failed.
            self
                .read_guest_memory(addr, count)
        } else {
            None
        }
    }

    /// Read bytes from guest virtual memory.
    ///
    /// # Arguments
    /// * `gva` - Guest virtual address to read from
    /// * `size` - Number of bytes to read
    ///
    /// # Returns
    /// * `Option<Vec<u8>>` - The read bytes if successful, None if the address is invalid
    pub fn write_guest_virtual_memory(&self, vcpu_id: usize, gva: usize, data: &[u8]) -> Option<()> {
        if let Some(vcpu) = self.vcpu(vcpu_id) {
            let (mut addr, mut count) = (gva, data.len());
            let page_table = vcpu.get_page_table_root();
            if page_table != 0.into() {
                let (paddr, _, size) = self
                    .get_page(page_table, addr as u64).ok()?;
                addr = paddr.as_usize();
                count = count.min(size as usize);
            }
            self.write_guest_memory(addr, &data[..count])
        } else {
            None
        }
    }
}
