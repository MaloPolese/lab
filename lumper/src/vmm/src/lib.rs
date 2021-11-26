// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

#![cfg(target_arch = "x86_64")]

extern crate libc;

extern crate linux_loader;
extern crate vm_memory;

use std::thread;
use std::{io, path::PathBuf};

use kvm_bindings::{kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VmFd};
use linux_loader::loader::{self, KernelLoaderResult};
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

mod cpu;
use cpu::{cpuid, mptable, Vcpu};
mod kernel;

#[derive(Debug)]

/// VMM errors.
pub enum Error {
    /// Failed to write boot parameters to guest memory.
    BootConfigure(linux_loader::configurator::Error),
    /// Error configuring the kernel command line.
    Cmdline(linux_loader::cmdline::Error),
    /// Failed to load kernel.
    KernelLoad(loader::Error),
    /// Invalid E820 configuration.
    E820Configuration,
    /// Highmem start address is past the guest memory end.
    HimemStartPastMemEnd,
    /// I/O error.
    IO(io::Error),
    /// Error issuing an ioctl to KVM.
    KvmIoctl(kvm_ioctls::Error),
    /// vCPU errors.
    Vcpu(cpu::Error),
    /// Memory error.
    Memory(vm_memory::Error),
}

/// Dedicated [`Result`](https://doc.rust-lang.org/std/result/) type.
pub type Result<T> = std::result::Result<T, Error>;

pub struct VMM {
    vm_fd: VmFd,
    kvm: Kvm,
    guest_memory: GuestMemoryMmap,
    vcpus: Vec<Vcpu>,
}

impl VMM {
    /// Create a new VMM.
    pub fn new() -> Result<Self> {
        // Open /dev/kvm and get a file descriptor to it.
        let kvm = Kvm::new().map_err(Error::KvmIoctl)?;

        // Create a KVM VM object.
        // KVM returns a file descriptor to the VM object.
        let vm_fd = kvm.create_vm().map_err(Error::KvmIoctl)?;

        let vmm = VMM {
            vm_fd,
            kvm,
            guest_memory: GuestMemoryMmap::default(),
            vcpus: vec![],
        };

        Ok(vmm)
    }

    pub fn configure_memory(&mut self, mem_size_mb: u32) -> Result<()> {
        Ok(())
    }

    pub fn configure_io(&mut self) -> Result<()> {
        // First, create the irqchip.
        // On `x86_64`, this _must_ be created _before_ the vCPUs.
        // It sets up the virtual IOAPIC, virtual PIC, and sets up the future vCPUs for local APIC.
        // When in doubt, look in the kernel for `KVM_CREATE_IRQCHIP`.
        // https://elixir.bootlin.com/linux/latest/source/arch/x86/kvm/x86.c
        self.vm_fd.create_irq_chip().map_err(Error::KvmIoctl)?;

        Ok(())
    }

    pub fn configure_vcpus(
        &mut self,
        num_vcpus: u8,
        kernel_load: KernelLoaderResult,
    ) -> Result<()> {
        mptable::setup_mptable(&self.guest_memory, num_vcpus)
            .map_err(|e| Error::Vcpu(cpu::Error::Mptable(e)))?;

        let base_cpuid = self
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::KvmIoctl)?;

        for index in 0..num_vcpus {
            let vcpu = Vcpu::new(&self.vm_fd, index.into()).map_err(Error::Vcpu)?;

            // Set CPUID.
            let mut vcpu_cpuid = base_cpuid.clone();
            cpuid::filter_cpuid(
                &self.kvm,
                index as usize,
                num_vcpus as usize,
                &mut vcpu_cpuid,
            );
            vcpu.configure_cpuid(&vcpu_cpuid).map_err(Error::Vcpu)?;

            // Configure MSRs (model specific registers).
            vcpu.configure_msrs().map_err(Error::Vcpu)?;

            // Configure regs, sregs and fpu.
            vcpu.configure_regs(kernel_load.kernel_load)
                .map_err(Error::Vcpu)?;
            vcpu.configure_sregs(&self.guest_memory)
                .map_err(Error::Vcpu)?;
            vcpu.configure_fpu().map_err(Error::Vcpu)?;

            // Configure LAPICs.
            vcpu.configure_lapic().map_err(Error::Vcpu)?;

            self.vcpus.push(vcpu);
        }

        Ok(())
    }

    pub fn run(&mut self) {}

    pub fn configure(&mut self, num_vcpus: u8, mem_size_mb: u32, kernel_path: &str) -> Result<()> {
        self.configure_memory(mem_size_mb)?;
        let kernel_load = kernel::kernel_setup(&self.guest_memory, PathBuf::from(kernel_path))?;
        self.configure_io()?;
        self.configure_vcpus(num_vcpus, kernel_load)?;

        Ok(())
    }
}
