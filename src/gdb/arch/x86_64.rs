use crate::{AxVMHal, AxVMRef};
use axvcpu::AxVCpuHal;
use gdbstub::target::TargetResult;

pub struct X86_64Target;

impl X86_64Target {
    pub fn new() -> Self {
        Self
    }
}

impl super::ArchTarget for X86_64Target {
    fn read_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &AxVMRef<H, U>,
        regs: &mut <gdbstub_arch::x86::X86_64_SSE as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            // regs[] order: RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, R8-R15
            // get_gpr() order: RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8-R15

            // Map general purpose registers
            let gpr_map = [0, 3, 1, 2, 6, 7, 5, 4]; // Maps regs[] index to get_gpr() index
            for (i, &gpr_idx) in gpr_map.iter().enumerate() {
                regs.regs[i] = vcpu.get_gpr(gpr_idx) as u64;
            }

            // Map R8-R15 registers (same order in both)
            for i in 8..16 {
                regs.regs[i] = vcpu.get_gpr(i) as u64;
            }

            // Special registers
            regs.rip = vcpu.get_pc() as u64;
        }
    }

    fn write_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &mut AxVMRef<H, U>,
        regs: &<super::Arch as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            // regs[] order: RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, R8-R15
            // set_gpr() order: RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8-R15

            // Map general purpose registers
            let gpr_map = [0, 3, 1, 2, 6, 7, 5, 4]; // Maps regs[] index to set_gpr() index
            for (i, &gpr_idx) in gpr_map.iter().enumerate() {
                vcpu.set_gpr(gpr_idx, regs.regs[i] as usize);
            }

            // Map R8-R15 registers (same order in both)
            for i in 8..16 {
                vcpu.set_gpr(i, regs.regs[i] as usize);
            }

            // Special registers
            vcpu.set_pc(regs.rip as usize);
        }
    }
}
