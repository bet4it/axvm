use crate::{AxVMHal, AxVMRef};
use axvcpu::AxVCpuHal;
use gdbstub_arch;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::AArch64Target as CurrentTarget;
#[cfg(target_arch = "aarch64")]
pub type Arch = gdbstub_arch::aarch64::AArch64;

#[cfg(target_arch = "riscv64")]
mod riscv;
#[cfg(target_arch = "riscv64")]
pub use riscv::RiscvTarget as CurrentTarget;
#[cfg(target_arch = "riscv64")]
pub type Arch = gdbstub_arch::riscv::Riscv64;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::X86_64Target as CurrentTarget;
#[cfg(target_arch = "x86_64")]
pub type Arch = gdbstub_arch::x86::X86_64_SSE;

pub trait ArchTarget {
    fn read_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &AxVMRef<H, U>,
        regs: &mut <Arch as gdbstub::arch::Arch>::Registers,
    );
    fn write_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &mut AxVMRef<H, U>,
        regs: &<Arch as gdbstub::arch::Arch>::Registers,
    );
}
