use crate::{AxVMHal, AxVMRef};
use axvcpu::AxVCpuHal;

pub struct AArch64Target;

impl AArch64Target {
    pub fn new() -> Self {
        Self
    }
}

impl super::ArchTarget for AArch64Target {
    fn read_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &AxVMRef<H, U>,
        regs: &mut <gdbstub_arch::aarch64::AArch64 as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            for i in 0..=30 {
                regs.x[i] = vcpu.get_gpr(i) as u64;
            }
            regs.sp = vcpu.get_gpr(31) as u64;
            regs.pc = vcpu.get_pc() as u64;
        }
    }

    fn write_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &mut AxVMRef<H, U>,
        regs: &<super::Arch as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            for i in 0..=30 {
                vcpu.set_gpr(i, regs.x[i] as usize);
            }
            vcpu.set_gpr(31, regs.sp as usize);
            vcpu.set_pc(regs.pc as usize);
        }
    }
}
