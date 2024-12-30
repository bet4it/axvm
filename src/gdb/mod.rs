use alloc::sync::Arc;
use axerrno::AxError;
use axvcpu::AxVCpuHal;
use gdbstub::{
    common::Signal,
    stub::{state_machine::GdbStubStateMachine, GdbStub},
    target::{Target, TargetResult},
};

use crate::{gdb::arch::ArchTarget, AxVM, AxVMHal, AxVMRef};

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
        // let bytes_read = self.vm.read_memory(start_addr, data)?;
        Ok(0)
    }

    fn write_addrs(
        &mut self,
        start_addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        data: &[u8],
    ) -> TargetResult<(), Self> {
        // self.vm.write_memory(start_addr, data)?;
        Ok(())
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
        info!("GDB server loop");
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
                        info!("GDB server: entering running state");
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
        info!("GDB server report");
        let gdbstub = self.inner_mut.gdb_state.lock().take();
        if let Some(gdb) = gdbstub {
            let mut target = self.inner_mut.gdb_target.lock().take().unwrap();
            let new_state = if let GdbStubStateMachine::Running(gdb_inner) = gdb {
                match gdb_inner
                    .report_stop(&mut target, gdbstub::stub::SingleThreadStopReason::DoneStep)
                {
                    Ok(gdb_state) => Some(gdb_state),
                    Err(_) => None,
                }
            } else {
                Some(gdb)
            };
            *self.inner_mut.gdb_target.lock() = Some(target);
            *self.inner_mut.gdb_state.lock() = new_state;
        }
    }
}
