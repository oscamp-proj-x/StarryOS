use alloc::{format, sync::Arc, vec::Vec};

use axfs::FsContext;
use axfs_ng_vfs::{
    DeviceId, NodePermission,
    path::{Path, PathBuf},
};
use crab_usb::DeviceInfo;
use starry_core::vfs::{Device, DirMapping, SimpleDir, SimpleFs};

mod usb;
mod usb_host;

mod uvc;

use usb::{UsbDevice, UsbDeviceType};

const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);

pub fn builder(fs: Arc<SimpleFs>, root: &mut DirMapping, gfs: &FsContext) {
    // USB devices
    let device_info_list = usb_host::get_device_list();
    if device_info_list.is_err() {
        warn!("Failed to get USB devices");
        return;
    }
    let device_info_list = device_info_list.unwrap();
    if device_info_list.len() == 0 {
        warn!("No USB devices found");
        return;
    }

    let mut usb_dir = DirMapping::new();
    let mut input_dir: Option<DirMapping> = None;
    let mut input_index = 0;
    let mut uvc_index = 0;

    for (i, device_info) in device_info_list.into_iter().enumerate() {
        let descriptor = device_info.descriptor();
        let device_id = format!("{:04x}:{:04x}", descriptor.vendor_id, descriptor.product_id,);

        // let mut device_info = device_info;
        let device = usb_host::open_device(&device_info);
        warn!("opened dev: {device:?}");

        let sys_usb_dev_path = format!("/sys/bus/usb/devices/1-{}/", i + 1);

        // 创建目录 /sys/bus/usb/devices/1-{index}
        let mut path = PathBuf::new();
        for comp in Path::new(&sys_usb_dev_path).components() {
            path.push(comp.as_str());
            if gfs.resolve(&path).is_err() {
                gfs.create_dir(&path, DIR_PERMISSION).unwrap();
            }
        }

        // 创建 相关描述 文件
        let dev_path = format!("{}/idVendor", sys_usb_dev_path);
        gfs.write(&dev_path, format!("{:04x}", descriptor.vendor_id))
            .unwrap();

        let dev_path = format!("{}/idProduct", sys_usb_dev_path);
        gfs.write(&dev_path, format!("{:04x}", descriptor.product_id))
            .unwrap();

        let dev_path = format!("{}/product", sys_usb_dev_path);
        gfs.write(&dev_path, "uvc\n").unwrap();

        let dev_path = format!("{}/manufacturer", sys_usb_dev_path);
        gfs.write(&dev_path, device.manufacturer().unwrap())
            .unwrap();

        let dev_path = format!("{}/busnum", sys_usb_dev_path);
        gfs.write(&dev_path, "1\n").unwrap();

        let dev_path = format!("{}/devnum", sys_usb_dev_path);
        gfs.write(&dev_path, format!("{}\n", i + 1)).unwrap();

        let dev_path = format!("{}/speed", sys_usb_dev_path);
        gfs.write(&dev_path, "480\n").unwrap();

        let usb_device = UsbDevice::new(&device_info, device);
        let usb_index = i as u32;
        warn!(
            "USB device: {}, index: {}, node_type: {:?}, device_type: {:?}",
            device_id, usb_index, usb_device.node_type, usb_device.r#type
        );

        // 动态生成USB描述符：设备描述符 + 所有配置描述符
        let mut descriptors = build_device_descriptor(&device_info);
        descriptors.extend(usb_device.configuration_descriptor.clone());
        warn!("configuration_descriptor: {:?}", descriptors);

        let dev_path = format!("{}/descriptors", sys_usb_dev_path);
        gfs.write(&dev_path, &descriptors).unwrap();

        let ops: Arc<Device>;
        match usb_device.r#type {
            UsbDeviceType::Keyboard => {
                ops = Device::new(
                    fs.clone(),
                    usb_device.node_type,
                    DeviceId::new(13, (input_index) as _),
                    usb_device.ops.unwrap(),
                );
                let input_dir = input_dir.get_or_insert(DirMapping::new());
                input_dir.add(format!("event{}", input_index), ops.clone());
                input_index += 1;
            }
            UsbDeviceType::Mouse => {
                ops = Device::new(
                    fs.clone(),
                    usb_device.node_type,
                    DeviceId::new(13, (input_index) as _),
                    usb_device.ops.unwrap(),
                );
                let input_dir = input_dir.get_or_insert(DirMapping::new());
                input_dir.add(format!("mice"), ops.clone());
                input_index += 1;
            }
            UsbDeviceType::UVC => {
                ops = Device::new(
                    fs.clone(),
                    usb_device.node_type,
                    DeviceId::new(81, (uvc_index) as _), /* 81是Linux中video4linux设备的标准主设备号 */
                    usb_device.ops.unwrap(),
                );
                root.add(format!("video{}", uvc_index), ops.clone());

                uvc_index += 1;
            }
            _ => {
                ops = Device::new(
                    fs.clone(),
                    usb_device.node_type,
                    DeviceId::new(13, (i + 1) as _),
                    usb_device.ops.unwrap(),
                );
            }
        }

        usb_dir.add(format!("{:03x}", usb_index + 1), ops);
    }

    let mut hub_dir = DirMapping::new();
    hub_dir.add("001", SimpleDir::new_maker(fs.clone(), Arc::new(usb_dir)));
    let mut bus_dir = DirMapping::new();
    bus_dir.add("usb", SimpleDir::new_maker(fs.clone(), Arc::new(hub_dir)));
    root.add("bus", SimpleDir::new_maker(fs.clone(), Arc::new(bus_dir)));

    if let Some(input_dir) = input_dir {
        root.add(
            "input",
            SimpleDir::new_maker(fs.clone(), Arc::new(input_dir)),
        );
    }
}

// 辅助函数：将u16转换为小端序的两个字节（低字节在前）
fn u16_to_le_bytes(value: u16) -> (u8, u8) {
    let low_byte = (value & 0xFF) as u8; // 拆分低8位
    let high_byte = ((value >> 8) & 0xFF) as u8; // 拆分高8位
    (low_byte, high_byte)
}

// 动态生成USB设备描述符
fn build_device_descriptor(device_info: &DeviceInfo) -> Vec<u8> {
    let mut dev_desc = Vec::with_capacity(18); // 预分配18字节，提升性能

    // ===== 按USB设备描述符标准逐字节push =====
    // 0: bLength - 描述符总长度（18字节）
    dev_desc.push(0x12);
    // 1: bDescriptorType - 设备描述符类型（0x01）
    dev_desc.push(0x01);
    // 2-3: bcdUSB - USB 2.1版本（0x0210，小端序：0x10, 0x02）
    let (usb_low, usb_high) = u16_to_le_bytes(0x0210);
    dev_desc.push(usb_low);
    dev_desc.push(usb_high);
    // 4: bDeviceClass - UVC杂项类（0xEF）
    dev_desc.push(0xEF);
    // 5: bDeviceSubClass - 子类型（0x02）
    dev_desc.push(0x02);
    // 6: bDeviceProtocol - 协议（0x01）
    dev_desc.push(0x01);
    // 7: bMaxPacketSize0 - 端点0最大包大小（64字节=0x40）
    dev_desc.push(0x40);
    // 8-9: idVendor - 厂商ID（动态从device_info取，小端序）
    let (vid_low, vid_high) = u16_to_le_bytes(device_info.descriptor().vendor_id);
    dev_desc.push(vid_low);
    dev_desc.push(vid_high);
    // 10-11: idProduct - 产品ID（动态从device_info取，小端序）
    let (pid_low, pid_high) = u16_to_le_bytes(device_info.descriptor().product_id);
    dev_desc.push(pid_low);
    dev_desc.push(pid_high);
    // 12-13: bcdDevice - 设备版本号（示例0x1111，可改为动态值）
    let (dev_ver_low, dev_ver_high) = u16_to_le_bytes(0x1111);
    dev_desc.push(dev_ver_low);
    dev_desc.push(dev_ver_high);
    // 14: iManufacturer - 厂商字符串索引（0x01）
    dev_desc.push(0x01);
    // 15: iProduct - 产品字符串索引（0x02）
    dev_desc.push(0x02);
    // 16: iSerialNumber - 序列号字符串索引（0x03）
    dev_desc.push(0x03);
    // 17: bNumConfigurations - 配置数量（0x01）
    dev_desc.push(0x01);

    dev_desc
}