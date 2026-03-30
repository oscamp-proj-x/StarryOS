extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;

use axfs_ng_vfs::{NodeFlags, NodeType, VfsResult};
pub use crab_usb::*;
use starry_core::vfs::DeviceOps;
use usb_if::descriptor::Class;

use crate::vfs::dev::usb::uvc::{UVC, UvcDevice};

#[derive(Debug)]
pub enum UsbDeviceType {
    Keyboard,    // 键盘
    Mouse,       // 鼠标
    MassStorage, // U 盘
    UVC,         // 摄像头
    Other,       // 其他设备
}

pub struct UsbDevice {
    pub class: Class,
    pub node_type: NodeType,
    pub r#type: UsbDeviceType,
    pub ops: Option<Arc<dyn DeviceOps>>,
    pub configuration_descriptor: Vec<u8>,
}

impl UsbDevice {
    pub fn new(device_info: &DeviceInfo, device: Device) -> Self {
        let mut usb = Self {
            class: Class::ClassInInterface,
            node_type: NodeType::CharacterDevice,
            r#type: UsbDeviceType::Other,
            ops: None,
            configuration_descriptor: Vec::new(),
        };

        usb.init_attrs(device_info);
        usb.build_device_ops(device_info, device);

        usb
    }

    fn init_attrs(&mut self, device_info: &DeviceInfo) {
        // Video (UVC)
        if UvcDevice::check(&device_info) {
            self.class = Class::Video;
            self.r#type = UsbDeviceType::UVC;
            return;
        }
    }

    fn build_device_ops(&mut self, device_info: &DeviceInfo, device: Device) {
        // 根据类型创建具体设备
        let ops: Arc<dyn DeviceOps> = match self.r#type {
            UsbDeviceType::UVC => {
                let uvc = UVC::new(device);
                self.configuration_descriptor = uvc.configuration_descriptor.clone();
                Arc::new(uvc)
            }
            _ => Arc::new(UsbDeviceOther::new(device_info)),
        };
        self.ops = Some(ops);
    }
}

pub struct UsbDeviceOther {}

impl UsbDeviceOther {
    pub fn new(device_info: &DeviceInfo) -> Self {
        Self {}
    }
}

impl DeviceOps for UsbDeviceOther {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        panic!("todo")
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        panic!("todo")
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        panic!("todo")
    }

    fn as_any(&self) -> &(dyn Any) {
        self
    }

    fn as_pollable(&self) -> Option<&dyn axpoll::Pollable> {
        None
    }

    fn mmap(&self) -> starry_core::vfs::DeviceMmap {
        starry_core::vfs::DeviceMmap::None
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}