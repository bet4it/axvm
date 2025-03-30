use alloc::sync::Arc;
use axerrno::AxError;
use axvcpu::AxVCpuHal;
use gdbstub::{
    common::Signal,
    stub::{GdbStub, state_machine::GdbStubStateMachine},
    target::{Target, TargetError, TargetResult},
};

use crate::{AxVM, AxVMHal, AxVMRef, gdb::arch::ArchTarget};

mod arch;

pub struct GdbTarget<H: AxVMHal, U: AxVCpuHal> {
    vm: AxVMRef<H, U>,
    target: arch::CurrentTarget,
}

impl<H: AxVMHal, U: AxVCpuHal> GdbTarget<H, U> {
    pub fn new(vm: &AxVMRef<H, U>) -> Self {
        Self {
            vm: Arc::clone(vm),
            target: arch::CurrentTarget::new(),
        }
    }
}

impl<H: AxVMHal, U: AxVCpuHal> Target for GdbTarget<H, U> {
    type Arch = arch::Arch;
    type Error = AxError;

    fn base_ops(&mut self) -> gdbstub::target::ext::base::BaseOps<'_, Self::Arch, Self::Error> {
        gdbstub::target::ext::base::BaseOps::SingleThread(self)
    }

    fn guard_rail_implicit_sw_breakpoints(&self) -> bool {
        true
    }
}

impl<H: AxVMHal, U: AxVCpuHal> gdbstub::target::ext::base::singlethread::SingleThreadBase
    for GdbTarget<H, U>
{
    fn read_registers(
        &mut self,
        regs: &mut <Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> TargetResult<(), Self> {
        self.target.read_registers(&self.vm, regs);
        Ok(())
    }

    fn write_registers(
        &mut self,
        regs: &<Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> TargetResult<(), Self> {
        self.target.write_registers(&mut self.vm, regs);
        Ok(())
    }

    fn read_addrs(
        &mut self,
        start_addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        data: &mut [u8],
    ) -> TargetResult<usize, Self> {
        let res = self.vm
            .read_guest_virtual_memory(0, start_addr as usize, data.len())
            .ok_or(TargetError::Errno(1))?;
        data.copy_from_slice(&res);
        Ok(res.len())
    }

    fn write_addrs(
        &mut self,
        start_addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        data: &[u8],
    ) -> TargetResult<(), Self> {
        self.vm
            .write_guest_virtual_memory(0, start_addr as usize, data)
            .ok_or(TargetError::Errno(1))

    }

    fn support_resume(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl<H: AxVMHal, U: AxVCpuHal> gdbstub::target::ext::base::singlethread::SingleThreadResume
    for GdbTarget<H, U>
{
    fn resume(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline(always)]
    fn support_single_step(&mut self) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadSingleStepOps<Self>> {
        Some(self)
    }
}

impl<H: AxVMHal, U: AxVCpuHal> gdbstub::target::ext::base::singlethread::SingleThreadSingleStep for GdbTarget<H, U> {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err(AxError::BadAddress);
        }
        self.target.step(&mut self.vm);
        Ok(())
    }
}

impl<H: AxVMHal, U: AxVCpuHal> AxVM<H, U> {
    pub fn gdbserver_init(self: &Arc<Self>, conn: crate::config::GdbConnection) {
        info!("Initializing GDB server");
        let mut target = GdbTarget::new(self);
        if let Ok(gdbstub) = GdbStub::new(conn).run_state_machine(&mut target) {
            info!("Started GDB server");
            *self.inner_mut.gdb_state.lock() = Some(gdbstub);
            *self.inner_mut.gdb_target.lock() = Some(target);
        } else {
            error!("Failed to start GDB server");
        }
    }

    pub fn gdbserver_loop(self: &Arc<Self>) {
        let gdbstub = self.inner_mut.gdb_state.lock().take();

        if let Some(mut gdb) = gdbstub {
            let mut target = self.inner_mut.gdb_target.lock().take().unwrap();
            loop {
                let next_state = match gdb {
                    GdbStubStateMachine::Idle(mut gdb_inner) => {
                        match gdb_inner.borrow_conn().read() {
                            Ok(byte) => match gdb_inner.incoming_data(&mut target, byte) {
                                Ok(new_state) => new_state,
                                Err(_) => {
                                    error!("GDB server error: failed to process incoming data");
                                    return;
                                }
                            },
                            Err(_) => {
                                error!("GDB server error: failed to read from connection");
                                return;
                            }
                        }
                    }
                    GdbStubStateMachine::Running(_) => {
                        // info!("GDB server: Running");
                        break;
                    }
                    GdbStubStateMachine::CtrlCInterrupt(_) => {
                        info!("GDB server: received Ctrl-C interrupt");
                        break;
                    }
                    GdbStubStateMachine::Disconnected(_) => {
                        info!("GDB server: disconnected");
                        break;
                    }
                };
                gdb = next_state;
            }

            *self.inner_mut.gdb_target.lock() = Some(target);
            *self.inner_mut.gdb_state.lock() = Some(gdb);
        }
    }

    pub fn gdbserver_report(self: &Arc<Self>) {
        let gdb = self.inner_mut.gdb_state.lock().take();
        let target = self.inner_mut.gdb_target.lock().take();

        if let Some((addr, saved_inst, saved_len)) = self.inner_mut.saved_inst.lock().take() {
            self.write_guest_virtual_memory(0, addr as usize, &saved_inst[..saved_len])
                .expect("Failed to restore instruction");
        }
        if let (Some(gdb_inner), Some(mut target_inner)) = (gdb, target) {
            let gdb = if let GdbStubStateMachine::Running(gdb_running) = gdb_inner {
                match gdb_running.report_stop(
                    &mut target_inner,
                    gdbstub::stub::SingleThreadStopReason::DoneStep,
                ) {
                    Ok(gdb) => Some(gdb),
                    Err(e) => {
                        warn!("Report stop error: {:?}", e);
                        return;
                    }
                }
            } else {
                Some(gdb_inner)
            };
            *self.inner_mut.gdb_state.lock() = gdb;
            *self.inner_mut.gdb_target.lock() = Some(target_inner);
            self.gdbserver_loop();
        }
    }
}
