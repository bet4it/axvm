use crate::{AxVMHal, AxVMRef};
use axvcpu::AxVCpuHal;
use raki::{BaseIOpcode, Decode, Isa, OpcodeKind, PrivOpcode, COpcode};
pub struct RiscvTarget;

impl RiscvTarget {
    pub fn new() -> Self {
        Self
    }
}

fn sign_extend(value: u16, bits: usize) -> i16 {
    let shift = 16 - bits;
    ((value << shift) as i16) >> shift
}

impl super::ArchTarget for RiscvTarget {
    fn read_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &AxVMRef<H, U>,
        regs: &mut <gdbstub_arch::riscv::Riscv64 as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            for i in 0..32 {
                regs.x[i] = vcpu.get_gpr(i) as u64;
            }
            regs.pc = vcpu.get_pc() as u64;
        }
    }

    fn write_registers<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &mut AxVMRef<H, U>,
        regs: &<super::Arch as gdbstub::arch::Arch>::Registers,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            for i in 0..32 {
                vcpu.set_gpr(i, regs.x[i] as usize);
            }
            vcpu.set_pc(regs.pc as usize);
        }
    }

    fn step<H: AxVMHal, U: AxVCpuHal>(
        &self,
        vm: &mut AxVMRef<H, U>,
    ) {
        if let Some(vcpu) = vm.vcpu(0) {
            let curr_pc = vcpu.get_pc() as u64;

            let (inst_data, is_compressed) = {
                let raw_data = vm.read_guest_virtual_memory(0, curr_pc as usize, 4)
                    .unwrap_or_else(|| panic!("Failed to read instruction at {:#x}", curr_pc));

                let is_compressed = raw_data.get(0).map_or(false, |b| (b & 0b11) != 0b11);
                let len = if is_compressed { 2 } else { 4 };

                if raw_data.len() < len {
                    panic!("Incomplete instruction read at {:#x} (expected {} bytes, got {})",
                           curr_pc, len, raw_data.len());
                }

                let mut buf = [0u8; 4];
                buf[..len].copy_from_slice(&raw_data[..len]);
                (buf, is_compressed)
            };

            let (inst, inst_len) = if is_compressed {
                let raw = u16::from_le_bytes(inst_data[..2].try_into().unwrap());
                (raw.decode(Isa::Rv64).unwrap(), 2)
            } else {
                let raw = u32::from_le_bytes(inst_data);
                (raw.decode(Isa::Rv64).unwrap(), 4)
            };

            let next_pc = match inst.opc {
                OpcodeKind::C(COpcode::JALR) => {
                    let rs1 = inst.rs1.expect("c.jalr requires rs1");
                    let base = vcpu.get_gpr(rs1) as u64;
                    let target = base & !1;
                    target
                }
                OpcodeKind::C(COpcode::JR) => {
                    let rs1 = inst.rs1.expect("c.jr requires rs1");
                    let base = vcpu.get_gpr(rs1) as u64;
                    let target = base & !1;
                    target
                }
                OpcodeKind::C(COpcode::J) => {
                    let offset = sign_extend(inst.imm.unwrap() as u16, 11);
                    curr_pc.wrapping_add(offset as u64)
                }

                OpcodeKind::C(COpcode::BEQZ) | OpcodeKind::C(COpcode::BNEZ) => {
                    let rs1 = inst.rs1.unwrap();
                    let offset = sign_extend(inst.imm.unwrap() as u16, 8);
                    let taken = match inst.opc {
                        OpcodeKind::C(COpcode::BEQZ) => vcpu.get_gpr(rs1) == 0,
                        OpcodeKind::C(COpcode::BNEZ) => vcpu.get_gpr(rs1) != 0,
                        _ => unreachable!(),
                    };
                    if taken { curr_pc.wrapping_add(offset as u64) } else { curr_pc + inst_len as u64 }
                }

                OpcodeKind::BaseI(op @ (BaseIOpcode::BEQ | BaseIOpcode::BNE |
                                     BaseIOpcode::BLT | BaseIOpcode::BGE |
                                     BaseIOpcode::BLTU | BaseIOpcode::BGEU)) => {
                    let rs1 = inst.rs1.unwrap();
                    let rs2 = inst.rs2.unwrap();
                    let rs1_val = vcpu.get_gpr(rs1);
                    let rs2_val = vcpu.get_gpr(rs2);

                    let taken = match op {
                        BaseIOpcode::BEQ => rs1_val == rs2_val,
                        BaseIOpcode::BNE => rs1_val != rs2_val,
                        BaseIOpcode::BLT => (rs1_val as i64) < (rs2_val as i64),
                        BaseIOpcode::BGE => (rs1_val as i64) >= (rs2_val as i64),
                        BaseIOpcode::BLTU => rs1_val < rs2_val,
                        BaseIOpcode::BGEU => rs1_val >= rs2_val,
                        _ => unreachable!(),
                    };

                    if taken {
                        curr_pc.wrapping_add(inst.imm.unwrap() as i64 as u64)
                    } else {
                        curr_pc + inst_len as u64
                    }
                }

                OpcodeKind::BaseI(BaseIOpcode::JAL) => {
                    curr_pc.wrapping_add(inst.imm.unwrap() as i64 as u64)
                }

                OpcodeKind::BaseI(BaseIOpcode::JALR) => {
                    let rs1 = inst.rs1.unwrap();
                    let offset = inst.imm.unwrap() as i64 as u64;
                    (vcpu.get_gpr(rs1) as u64 + offset) & !1
                }
                OpcodeKind::Priv(PrivOpcode::SRET) => {
                    0x00000000000130ee
                }

                _ => curr_pc + inst_len as u64
            };

            // 设置断点时保存长度信息
            let mut target_inst = [0u8; 4];
            let bp_len = if (next_pc & 0b11) == 0b11 { 4 } else { 2 };
            let res = vm.read_guest_virtual_memory(0, next_pc as usize, bp_len)
                .unwrap_or_else(|| panic!("Failed to read {}-byte instruction at {:#x}", bp_len, next_pc));
            target_inst[..bp_len].copy_from_slice(&res);

            *vm.inner_mut.saved_inst.lock() = Some((next_pc, target_inst, bp_len));

            match bp_len {
                2 => vm.write_guest_virtual_memory(0, next_pc as usize, &[0x02, 0x90]), // c.ebreak
                4 => vm.write_guest_virtual_memory(0, next_pc as usize, &[0x73, 0x00, 0x10, 0x00]), // ebreak
                _ => unreachable!()
            }.expect("Failed to write ebreak");
        }
    }
}
