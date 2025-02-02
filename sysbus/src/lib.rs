// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
//
// Copyright (c) 2023 China Telecom Co.,Ltd. All rights reserved.
// 
// Modifications made by China Telecom Co.,Ltd:
// - Modify sysbus for risc-v architecture
//
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

pub mod error;
pub use error::SysBusError;
use std::fmt;
use std::sync::{Arc, Mutex};
use address_space::{AddressSpace, GuestAddress, Region, RegionIoEventFd, RegionOps};
pub use anyhow::{bail, Context, Result};
use vmm_sys_util::eventfd::EventFd;

// According to the PLIC document, IRQ number 0 is not used
#[cfg(target_arch = "riscv64")]
pub const IRQ_BASE: i32 = 1;
#[cfg(target_arch = "riscv64")]
pub const IRQ_MAX: i32 = 1024;

pub struct SysBus {
    pub sys_mem: Arc<AddressSpace>,
    pub devices: Vec<Arc<Mutex<dyn SysBusDevOps>>>,
    pub free_irqs: (i32, i32),
    pub min_free_irq: i32,
    pub mmio_region: (u64, u64),
    pub min_free_base: u64,
}

impl fmt::Debug for SysBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
        let debug = f
            .debug_struct("SysBus")
            .field("sys_mem", &self.sys_mem)
            .field("free_irqs", &self.free_irqs)
            .field("min_free_irq", &self.min_free_irq)
            .field("mmio_region", &self.mmio_region)
            .field("min_free_base", &self.min_free_base)
            .finish();
        debug
    }
}

impl SysBus {
    pub fn new(
        sys_mem: &Arc<AddressSpace>,
        free_irqs: (i32, i32),
        mmio_region: (u64, u64),
    ) -> Self {
        Self {
            sys_mem: sys_mem.clone(),
            devices: Vec::new(),
            free_irqs,
            min_free_irq: free_irqs.0,
            mmio_region,
            min_free_base: mmio_region.0,
        }
    }

    pub fn build_region_ops<T: 'static + SysBusDevOps>(&self, dev: &Arc<Mutex<T>>) -> RegionOps {
        let cloned_dev = dev.clone();
        let read_ops = move |data: &mut [u8], addr: GuestAddress, offset: u64| -> bool {
            cloned_dev.lock().unwrap().read(data, addr, offset)
        };

        let cloned_dev = dev.clone();
        let write_ops = move |data: &[u8], addr: GuestAddress, offset: u64| -> bool {
            cloned_dev.lock().unwrap().write(data, addr, offset)
        };

        RegionOps {
            read: Arc::new(read_ops),
            write: Arc::new(write_ops),
        }
    }

    pub fn attach_device<T: 'static + SysBusDevOps>(
        &mut self,
        dev: &Arc<Mutex<T>>,
        region_base: u64,
        region_size: u64,
    ) -> Result<()> {
        let region_ops = self.build_region_ops(dev);
        let region = Region::init_io_region(region_size, region_ops);
        let locked_dev = dev.lock().unwrap();

        region.set_ioeventfds(&locked_dev.ioeventfds());
        match locked_dev.get_type() {
            SysBusDevType::Serial if cfg!(target_arch = "x86_64") => {
                #[cfg(target_arch = "x86_64")]
                self.sys_io
                    .root()
                    .add_subregion(region, region_base)
                    .with_context(|| {
                        format!(
                            "Failed to register region in I/O space: offset={},size={}",
                            region_base, region_size
                        )
                    })?;
            }
            _ => self
                .sys_mem
                .root()
                .add_subregion(region, region_base)
                .with_context(|| {
                    format!(
                        "Failed to register region in memory space: offset={},size={}",
                        region_base, region_size
                    )
                })?,
        }

        self.devices.push(dev.clone());
        Ok(())
    }

    pub fn attach_dynamic_device<T: 'static + SysBusDevOps>(
        &mut self,
        dev: &Arc<Mutex<T>>,
    ) -> Result<()> {
        self.devices.push(dev.clone());
        Ok(())
    }
}

#[derive(Copy, Clone)]
pub struct SysRes {
    pub region_base: u64,
    pub region_size: u64,
    pub irq: i32,
}

impl Default for SysRes {
    fn default() -> Self {
        Self {
            region_base: 0,
            region_size: 0,
            irq: -1,
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Eq, PartialEq)]
pub enum SysBusDevType {
    Serial,
    Rtc,
    VirtioMmio,
    #[cfg(target_arch = "riscv64")]
    Plic,
    FwCfg,
    Ramfb,
    PcieMem,
    Others,
}

/// Operations for sysbus devices.
pub trait SysBusDevOps: Send {
    /// Read function of device.
    ///
    /// # Arguments
    ///
    /// * `data` - A u8-type array.
    /// * `base` - Base address of this device.
    /// * `offset` - Offset from base address.
    fn read(&mut self, data: &mut [u8], base: GuestAddress, offset: u64) -> bool;

    /// Write function of device.
    ///
    /// # Arguments
    ///
    /// * `data` - A u8-type array.
    /// * `base` - Base address of this device.
    /// * `offset` - Offset from base address.
    fn write(&mut self, data: &[u8], base: GuestAddress, offset: u64) -> bool;

    fn ioeventfds(&self) -> Vec<RegionIoEventFd> {
        Vec::new()
    }

    fn interrupt_evt(&self) -> Option<&EventFd> {
        None
    }

    fn set_irq(&mut self, sysbus: &mut SysBus) -> Result<i32> {
        let irq = sysbus.min_free_irq;
        if irq > sysbus.free_irqs.1 {
            bail!("IRQ number exhausted.");
        }

        match self.interrupt_evt() {
            None => Ok(-1_i32),
            Some(evt) => {
          //      KVM_FDS.load().register_irqfd(evt, irq as u32)?;
                sysbus.min_free_irq = irq + 1;
                Ok(irq)
            }
        }
    }

    fn get_sys_resource(&mut self) -> Option<&mut SysRes> {
        None
    }

    fn set_sys_resource(
        &mut self,
        sysbus: &mut SysBus,
        region_base: u64,
        region_size: u64,
    ) -> Result<()> {
        let irq = self.set_irq(sysbus)?;
        if let Some(res) = self.get_sys_resource() {
            res.region_base = region_base;
            res.region_size = region_size;
            res.irq = irq;
            return Ok(());
        }
        bail!("Failed to get sys resource.");
    }

    fn get_type(&self) -> SysBusDevType {
        SysBusDevType::Others
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
}

// impl AmlBuilder for SysBus {
//     fn aml_bytes(&self) -> Vec<u8> {
//         let mut scope = AmlScope::new("_SB");
//         self.devices.iter().for_each(|dev| {
//             scope.append(&dev.lock().unwrap().aml_bytes());
//         });

//         scope.aml_bytes()
//     }
// }
