extern crate alloc;
use alloc::vec::Vec;
use core::{ptr::NonNull, time::Duration};

use axerrno::AxError;
use axhal::{
    mem::{PhysAddr, phys_to_virt},
    paging::MappingFlags,
};
use axmm::kernel_aspace;
use axtask::spawn_with_name;
use crab_usb::err::USBError;
pub use crab_usb::*;
// use dma_api::{DmaDirection, DmaError, DmaHandle, DmaMapHandle};
use mbarrier::mb;
use spin::{Mutex, Once};

//  引入 axdriver 提供的提货接口
use axdriver_dyn::blk::rockchip::build_dwc3_engine;

static USB_HOST: Once<Mutex<USBHost>> = Once::new();
static COHERENT_BUFFERS: spin::Mutex<alloc::vec::Vec<(axhal::mem::VirtAddr, usize)>> = spin::Mutex::new(alloc::vec::Vec::new());

pub struct KernelImpl;

// 🎯 暴露 DMA 操作接口，让 axdriver 能用它来组装引擎
pub static OS_USB_DMA_OP: KernelImpl = KernelImpl;

impl KernelOp for KernelImpl {
    fn delay(&self, duration: Duration) {
        // warn!("[USB] sleep {} ms", duration.as_millis());
        axhal::time::busy_wait(duration);
    }
}

// 保留原有的优良 iomap 辅助函数 (这在以后的 PCI 设备等也会很有用)
#[allow(dead_code)] // 暂时如果没用到可以加这个忽略警告
fn iomap(paddr: PhysAddr, size: usize) -> Result<NonNull<u8>, AxError> {
    warn!(
        "[USB] iomap: paddr=0x{:x}, size=0x{:x}",
        paddr.as_usize(),
        size
    );
    let vaddr = phys_to_virt(paddr);
    warn!("[USB] phys_to_virt => vaddr = 0x{:x}", vaddr.as_usize());

    let mut g = kernel_aspace().lock();

    if let Err(e) = g.map_linear(
        vaddr,
        paddr,
        size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
    ) {
        if !matches!(e, AxError::AlreadyExists) {
            return Err(e);
        }
    }

    g.protect(
        vaddr,
        size,
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
    )?;

    warn!(
        "[USB] mapping protect OK, final vaddr = 0x{:x}",
        vaddr.as_usize()
    );

    mb();

    Ok(unsafe { NonNull::new_unchecked(vaddr.as_mut_ptr()) })
}

fn flush_cache(vaddr: axhal::mem::VirtAddr, size: usize) {
    if size == 0 { return; }
    unsafe {
        let line_size = 64; // RK3588 Cortex-A76/A55 的 Cache Line 大小
        let start = vaddr.as_usize() & !(line_size - 1);
        let end = vaddr.as_usize() + size;
        for addr in (start..end).step_by(line_size) {
            core::arch::asm!("dc civac, {}", in(reg) addr);
        }
        core::arch::asm!("dsb sy", "isb");
    }
}

// 完整的 DmaOp 实现
impl dma_api::DmaOp for KernelImpl {
    fn page_size(&self) -> usize {
        0x1000
    }

    unsafe fn map_single(
        &self,
        _dma_mask: u64,
        addr: core::ptr::NonNull<u8>,
        size: core::num::NonZeroUsize,
        align: usize,
        _direction: dma_api::DmaDirection,
    ) -> Result<dma_api::DmaMapHandle, dma_api::DmaError> {
        let size = size.get();
        let vaddr = axhal::mem::VirtAddr::from(addr.as_ptr() as usize);
        
        // 发送前刷脏数据，保证内存里是最新的
        flush_cache(vaddr, size); 

        let phys = axhal::mem::virt_to_phys(vaddr).as_usize() as u64;
        let layout = core::alloc::Layout::from_size_align(size, align.max(1)).unwrap();
        
        Ok(unsafe {dma_api::DmaMapHandle::new(addr, phys.into(), layout, None)})
    }

    unsafe fn unmap_single(&self, handle: dma_api::DmaMapHandle) {
        let size = handle.layout().size();
        
        // 🚨 完美修复：使用 dma_addr() 获取 DMA 物理地址
        let phys_u64: u64 = handle.dma_addr().into(); 
        let vaddr = axhal::mem::phys_to_virt(axhal::mem::PhysAddr::from(phys_u64 as usize));
        
        // 接收后使 Cache 无效，强迫 CPU 看到那 18 个字节的设备描述符！
        flush_cache(vaddr, size);
    }

    unsafe fn alloc_coherent(
        &self,
        dma_mask: u64,
        layout: core::alloc::Layout,
    ) -> Option<dma_api::DmaHandle> {
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        let ptr = core::ptr::NonNull::new(ptr)?;
        let vaddr = axhal::mem::VirtAddr::from(ptr.as_ptr() as usize);
        let phys = axhal::mem::virt_to_phys(vaddr).as_usize() as u64;
        
        if (phys & !dma_mask) != 0 || ((phys + layout.size() as u64 - 1) & !dma_mask) != 0 {
            unsafe { alloc::alloc::dealloc(ptr.as_ptr(), layout) };
            return None;
        }
        
        // 加入全局跟踪队列
        COHERENT_BUFFERS.lock().push((vaddr, layout.size()));
        
        Some(unsafe { dma_api::DmaHandle::new(ptr, phys.into(), layout) })
    }

    unsafe fn dealloc_coherent(&self, handle: dma_api::DmaHandle) {
        let vaddr = axhal::mem::VirtAddr::from(handle.as_ptr().as_ptr() as usize);
        
        // 从跟踪队列中移除
        COHERENT_BUFFERS.lock().retain(|&(v, _)| v != vaddr);
        
        unsafe { alloc::alloc::dealloc(handle.as_ptr().as_ptr(), handle.layout()) };
    }
}

// 🎯 这是清理后唯一的、正确的 get_usb_host
fn get_usb_host() -> &'static Mutex<USBHost> {
    USB_HOST.call_once(|| {
        warn!("[USB] VFS 向 axdriver 请求组装 DWC3 发动机...");
        
        // 核心联动：把 DMA_OP 传下去，把组装好的 USBHost 拿上来！
        let mut host = build_dwc3_engine(&OS_USB_DMA_OP).expect("[USB] 获取 DWC3 引擎失败");
        
        warn!("[USB] DWC3 引擎获取成功，准备启动后台状态机...");
        // 启动后台事件轮询线程（保留你同事的神来之笔）
        let event_handler = host.create_event_handler();
        spawn_with_name(
            move || {
                warn!("[USB] usb_event_handler started");
                loop {
                    let _ = event_handler.handle_event();
                    core::hint::spin_loop();
                }
            },
            "usb_event_handler".into(),
        );
        spin_on::spin_on(async move {
            warn!("[USB] calling host.init()");
            host.init().await.expect("host.init failed!");
            warn!("[USB] host.init OK");

        // 👇 ================= 战术补丁 V3：协议解析 + 智能上电 ================= 👇
            unsafe {
                let dwc3_base = 0xffff9000fc400000usize;
                let op_base = dwc3_base + 0x20;
                
                // 🌟 1. 顺藤摸瓜：解析 xECP，找出硬件真实的端口血统
                let hccparams1 = core::ptr::read_volatile((dwc3_base + 0x10) as *const u32);
                let mut xecp = (hccparams1 >> 16) & 0xffff;
                warn!("[USB] 🔍 开始解析 xHCI 扩展能力链表...");
                
                while xecp != 0 {
                    let ext_addr = dwc3_base + (xecp as usize * 4);
                    let val = core::ptr::read_volatile(ext_addr as *const u32);
                    let cap_id = val & 0xff;
                    let next_ptr = (val >> 8) & 0xff;
                    
                    if cap_id == 2 { // ID 2 = Supported Protocol Capability
                        let rev = (val >> 16) & 0xffff;
                        let ports = core::ptr::read_volatile((ext_addr + 8) as *const u32);
                        let port_offset = ports & 0xff;
                        let port_count = (ports >> 8) & 0xff;
                        let is_usb3 = rev >= 0x0300;
                        warn!("[USB] 🎯 协议鉴定: {} 协议 (Rev {:#04x}) -> 分配了 {} 个端口 (起始端口: PORT{})", 
                            if is_usb3 { "USB3 (SuperSpeed)" } else { "USB2 (HighSpeed)" }, 
                            rev, port_count, port_offset
                        );
                    }
                    if next_ptr == 0 { break; }
                    xecp += next_ptr;
                }

                // 🌟 2. 遍历并给真正活着的端口上电
                let hcsparams1 = core::ptr::read_volatile((dwc3_base + 0x04) as *const u32);
                let max_ports = hcsparams1 & 0xff; 
                
                for port in 0..(max_ports as usize) {
                    let portsc_addr = (op_base + 0x400 + port * 0x10) as *mut u32;
                    let mut portsc = core::ptr::read_volatile(portsc_addr);
                    
                    // 过滤掉硬件上不存在的“幽灵端口”（全 0 的就是假端口）
                    if portsc == 0 { continue; }
                    
                    if (portsc & (1 << 9)) == 0 {
                        portsc |= 1 << 9;
                        core::ptr::write_volatile(portsc_addr, portsc);
                        warn!("[USB] ⚡ 强制给活着的 PORT{} 上电完毕！状态: {:#010x}", port + 1, portsc);
                    }
                }
            }
            // 👆 ================= 补丁结束 ================= 👆

            Mutex::new(host)
        })
    })
}

// =========================================
// API 接口保持不变
// =========================================
pub fn get_device_list() -> Result<Vec<DeviceInfo>, USBError> {
    warn!("[USB] get usb host");
    let mut host = get_usb_host().lock();
    warn!("[USB] get usb list");
    spin_on::spin_on(async {
        
        warn!("[USB] 等待设备连接 (最多10秒)...");
        let mut connected = false;
        for _ in 0..50 {
            unsafe {
                let phys_base = axhal::mem::PhysAddr::from(0xfc400000);
                if let Ok(vaddr) = iomap(phys_base, 0x10000) {
                    let base_ptr = vaddr.as_ptr() as *const u8;
                    
                    // 读所有4个 port
                    for port in 0..4 {
                        let offset = 0x20 + 0x400 + port * 0x10;
                        let portsc = core::ptr::read_volatile(
                            base_ptr.add(offset) as *const u32
                        );
                        let ccs   = portsc & 1;
                        let ped   = (portsc >> 1) & 1;
                        let pls   = (portsc >> 5) & 0xf;
                        let pp    = (portsc >> 9) & 1;
                        let speed = (portsc >> 10) & 0xf;
                        let csc   = (portsc >> 17) & 1;
                        warn!("PORT{}: {:#010x} CCS={} PED={} PLS={} PP={} Speed={} CSC={}",
                            port+1, portsc, ccs, ped, pls, pp, speed, csc);
                        
                        if ccs == 1 {
                            connected = true;
                        }
                    }
                    
                    if connected {
                        warn!("✅ 检测到设备连接！");
                        break;
                    }
                }
            }
            axtask::future::sleep(Duration::from_millis(200)).await;
        }
        
        if !connected {
            warn!("⚠️ 10秒内未检测到设备");
        }

        axtask::future::sleep(Duration::from_millis(200)).await;

        warn!("[USB] probe_devices");
        let ls = host.probe_devices().await?;
        warn!("[USB] collect, found {} devices", ls.len());
        Ok(ls)
    })
}

pub fn open_device(info: &DeviceInfo) -> Device {
    warn!("[USB] open device by host");
    let mut host = get_usb_host().lock();

    spin_on::spin_on(async { host.open_device(&info).await.unwrap() })
}