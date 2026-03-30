extern crate alloc;

use alloc::sync::Arc;
use core::any::Any;

use axfs::{FS_CONTEXT, FileFlags, OpenOptions};
use axfs_ng_vfs::{NodeFlags, VfsError, VfsResult};
use axhal::asm::user_copy;
use axtask::{TaskInner, future::block_on, spawn_task};
pub use crab_usb::*;
use spin::Mutex;
use starry_core::vfs::DeviceOps;

use super::*;
use crate::{io::IoVectorBufIo, vfs::dev::usb::uvc::frame::FrameEvent};

pub struct UVC {
    uvc: Arc<Mutex<UvcDevice>>,
    pub configuration_descriptor: Vec<u8>,

    frame_events: Arc<Mutex<Vec<FrameEvent>>>,
    cur_frame_event: Mutex<Option<FrameEvent>>,
    cur_frame_data_offset: Mutex<usize>,
    cur_fid: Mutex<u8>,

    waiting_urbs: Mutex<
        Vec<(
            usize,                    // 用户空间URB指针
            UsbdevfsUrb,              // URB结构体
            Vec<IsoPacketDescriptor>, // 等时包描述符数组
            usize,                    // 等时包描述符数组的用户空间地址
        )>,
    >,
    completed_urbs: Mutex<
        Vec<(
            usize,       // 用户空间URB指针
            UsbdevfsUrb, // URB结构体
        )>,
    >,
}

impl UVC {
    pub fn new(device: Device) -> Self {
        spin_on::spin_on(async {
            let mut uvc = UvcDevice::new(device).await.unwrap();

            // 获取设备信息
            let device_info_str = uvc.get_device_info().await.unwrap();
            warn!("Device info: {}", device_info_str);

            // 获取支持的视频格式
            let formats = uvc.get_supported_formats().await.unwrap();
            warn!("Supported formats:");
            for format in &formats {
                warn!("  {:?}", format);
            }

            // 设置视频格式 (选择第一个可用格式)
            if let Some(format) = formats.first() {
                warn!("Setting format: {:?}", format);
                uvc.set_format(format.clone()).await.unwrap();
            } else {
                error!("No supported formats available");
            }

            warn!("start_streaming");

            let mut stream = uvc.start_streaming().await.unwrap();
            warn!("start_streaming ok");
            let frame_events = Arc::new(Mutex::new(Vec::with_capacity(3)));
            let task_frame_events = frame_events.clone();

            let task_inner = TaskInner::new(
                move || {
                    stream_loop(&mut stream, task_frame_events);
                },
                "uvc_streaming".into(),
                starry_core::config::KERNEL_STACK_SIZE,
            );
            spawn_task(task_inner);

            let configuration_descriptor = uvc.get_full_configuration_descriptor().await.unwrap();

            UVC {
                uvc: Arc::new(Mutex::new(uvc)),
                configuration_descriptor,

                frame_events,
                cur_frame_event: Mutex::new(None),
                cur_frame_data_offset: Mutex::new(0),
                cur_fid: Mutex::new(0),

                waiting_urbs: Mutex::new(Vec::new()),
                completed_urbs: Mutex::new(Vec::new()),
            }
        })
    }
}

impl DeviceOps for UVC {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        warn!("read_at: {buf:?}, {offset}");
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        warn!("write_at: {buf:?}, {offset}");
        Ok(0)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        let ioc_type = ((cmd >> 8) & 0xff) as u8 as char;
        let ioc_nr = cmd & 0xff;

        warn!("ioctl: cmd={:#x}, type='{}', nr={}", cmd, ioc_type, ioc_nr);

        match ioc_type {
            'U' => match ioc_nr {
                USBDEVFS_GET_CAPABILITIES_NR => {
                    warn!("USBDEVFS_GET_CAPABILITIES_NR");
                    let resp = UsbdevfsCapabilities {
                        capabilities: 0x01 | 0x02 | 0x10, /* USBDEVFS_CAP_ZERO_PACKET | USBDEVFS_CAP_NO_PACKET_SIZE_LIM | USBDEVFS_CAP_SETINTERFACE */
                        reserved: [0; 15],
                    };

                    copy_to_user(
                        arg as *mut u8,
                        &resp as *const _ as *const u8,
                        core::mem::size_of::<UsbdevfsCapabilities>(),
                    )?;
                    Ok(0)
                }
                USBDEVFS_GET_DRIVER_NR => {
                    warn!("USBDEVFS_GET_DRIVER_NR");
                    let driver_name = b"uvcvideo\0";
                    let mut driver = [0u8; 255];
                    driver[..driver_name.len()].copy_from_slice(driver_name);

                    let resp = UsbdevfsDriver {
                        interface: 0,
                        driver,
                    };

                    copy_to_user(
                        arg as *mut u8,
                        &resp as *const _ as *const u8,
                        core::mem::size_of::<UsbdevfsDriver>(),
                    )?;

                    Ok(0)
                }
                USBDEVFS_CLAIMINTERFACE_NR => {
                    warn!("USBDEVFS_CLAIMINTERFACE_NR");
                    Ok(0)
                }
                USBDEVFS_RELEASEINTERFACE_NR => {
                    warn!("USBDEVFS_RELEASEINTERFACE_NR");
                    Ok(0)
                }
                USBDEVFS_RESETEP_NR => {
                    warn!("USBDEVFS_RESETEP_NR");
                    Ok(0)
                }
                USBDEVFS_SUBMITURB_NR => {
                    warn!("USBDEVFS_SUBMITURB_NR");

                    let mut req = UsbdevfsUrb::default();
                    copy_from_user(
                        &mut req as *mut _ as *mut u8,
                        arg as *const u8,
                        core::mem::size_of::<UsbdevfsUrb>(),
                    )?;

                    // 解析端点号
                    let endpoint_num = req.endpoint & 0x7F;
                    let direction = (req.endpoint >> 7) & 0x1; // 0=OUT, 1=IN

                    warn!(
                        "USBDEVFS_SUBMITURB: type={}, endpoint={}/{}, len={}, flags=0x{:x} \
                         direction: {}",
                        req.urb_type,
                        req.endpoint,
                        endpoint_num,
                        req.buffer_length,
                        req.flags,
                        if direction == 0 { "OUT" } else { "IN" },
                    );

                    match req.urb_type {
                        USBDEVFS_URB_TYPE_ISO => {
                            // 解析端点号
                            let endpoint_num = req.endpoint & 0x7F;
                            let direction = (req.endpoint >> 7) & 0x1;

                            warn!(
                                "等时传输 endpoint={}/{} ({}), buffer_length={}, \
                                 number_of_packets={}",
                                req.endpoint,
                                endpoint_num,
                                if direction == 0 { "OUT" } else { "IN" },
                                req.buffer_length,
                                req.number_of_packets
                            );

                            let uvc_guard = self.uvc.lock();
                            let status = uvc_guard.get_state();
                            warn!("uvc status: {:?}", status);

                            match (direction, req.number_of_packets) {
                                (1, 1..=i32::MAX) => {
                                    // 对于UVC视频流，通常是IN传输（设备到主机）

                                    // 计算等时包描述符的起始位置
                                    // 等时包描述符紧跟在URB结构之后
                                    let iso_desc_offset = core::mem::size_of::<UsbdevfsUrb>();
                                    let iso_desc_ptr = arg as usize + iso_desc_offset;

                                    // 读取所有等时包描述符
                                    let iso_descs_size = (req.number_of_packets as usize)
                                        * core::mem::size_of::<IsoPacketDescriptor>();
                                    let mut iso_descs: Vec<IsoPacketDescriptor> =
                                        Vec::with_capacity(req.number_of_packets as usize);
                                    unsafe {
                                        iso_descs.set_len(req.number_of_packets as usize);
                                    }

                                    // 计算描述符数组的用户空间地址
                                    let iso_descs_user_ptr = (arg as usize
                                        + core::mem::size_of::<UsbdevfsUrb>())
                                        as *const u8;

                                    copy_from_user(
                                        iso_descs.as_mut_ptr() as *mut u8,
                                        iso_desc_ptr as *const u8,
                                        iso_descs_size,
                                    )?;

                                    self.waiting_urbs.lock().push((
                                        arg,
                                        req,
                                        iso_descs,
                                        iso_descs_user_ptr as usize,
                                    ));
                                    return Ok(0);
                                }
                                _ => {
                                    warn!(
                                        "不支持的等时传输 direction: {}, number_of_packets: {}",
                                        direction, req.number_of_packets
                                    );
                                }
                            }
                        }
                        USBDEVFS_URB_TYPE_CONTROL => {
                            warn!("USBDEVFS_URB_TYPE_CONTROL");

                            let mut setup = UsbControlSetup::default();
                            copy_from_user(
                                &mut setup as *mut _ as *mut u8,
                                req.buffer as *const u8,
                                core::mem::size_of::<UsbControlSetup>(),
                            )?;

                            // 解析请求类型
                            let request_type = setup.bmRequestType & USB_TYPE_MASK;
                            let request_dir = setup.bmRequestType & USB_DIR_IN;
                            warn!(
                                "控制请求: bmRequestType=0x{:02x}, bRequest={}, wValue=0x{:04x}, \
                                 wIndex=0x{:04x}, wLength={}, 0x{:02x}, 方向: {}",
                                setup.bmRequestType,
                                setup.bRequest,
                                setup.wValue,
                                setup.wIndex,
                                setup.wLength,
                                request_type,
                                if request_dir == USB_DIR_IN {
                                    "IN"
                                } else {
                                    "OUT"
                                }
                            );

                            // 处理请求
                            match request_type {
                                USB_TYPE_STANDARD => match setup.bRequest {
                                    USB_REQ_GET_DESCRIPTOR => {
                                        let desc_type = (setup.wValue >> 8) as u8;
                                        let desc_index = setup.wValue as u8;

                                        warn!(
                                            "GET_DESCRIPTOR: type={}, index={}",
                                            desc_type, desc_index
                                        );

                                        match (desc_type, desc_index) {
                                            (USB_DT_STRING, 0) => {
                                                // 字符串描述符请求
                                                // 索引0：语言ID列表
                                                warn!("字符串描述符索引0：语言ID列表");

                                                // 准备响应数据：英语(0x0409)
                                                let response = vec![
                                                    0x04, // 长度：4字节
                                                    0x03, // 描述符类型：字符串
                                                    0x09, 0x04, // 语言ID：0x0409 (英语)
                                                ];

                                                // 将数据复制到URB缓冲区
                                                let data_offset =
                                                    core::mem::size_of::<UsbControlSetup>();
                                                let data_ptr =
                                                    unsafe { req.buffer.add(data_offset) };

                                                let copy_len =
                                                    response.len().min(setup.wLength as usize);
                                                copy_to_user(
                                                    data_ptr,
                                                    response.as_ptr(),
                                                    copy_len,
                                                )?;

                                                req.status = 0;
                                                req.actual_length = copy_len as i32;

                                                // 将URB放入已完成队列
                                                self.completed_urbs.lock().push((arg, req));
                                                return Ok(0);
                                            }
                                            _ => {
                                                warn!("未支持的描述符类型: {}", desc_type);
                                            }
                                        }
                                    }
                                    _ => {
                                        warn!("未支持的标准请求: {}", setup.bRequest);
                                    }
                                },
                                USB_TYPE_CLASS => match (setup.bRequest, request_dir) {
                                    (0x01, USB_DIR_OUT) => {
                                        let selector = (setup.wValue >> 8) as u8; // 控制选择器
                                        let entity_id = setup.wValue as u8; // 实体ID
                                        let interface = setup.wIndex as u8; // 接口号

                                        let data_offset = core::mem::size_of::<UsbControlSetup>();
                                        let data_ptr = unsafe { req.buffer.add(data_offset) };

                                        let mut data_buffer =
                                            Vec::with_capacity(setup.wLength as usize);
                                        unsafe {
                                            data_buffer.set_len(setup.wLength as usize);
                                        }
                                        copy_from_user(
                                            data_buffer.as_mut_ptr(),
                                            data_ptr,
                                            setup.wLength as usize,
                                        )?;

                                        warn!(
                                            "UVC: SET_CUR 请求，selector={:02X}, interface={}, \
                                             entity_id={}, wLength={}, data: {:02X?}",
                                            selector,
                                            interface,
                                            entity_id,
                                            setup.wLength,
                                            data_buffer
                                        );
                                        debug_probe_control(&data_buffer);

                                        // 更新用户空间的 status 和 actual_length
                                        unsafe {
                                            let user_urb_ptr = arg as *mut UsbdevfsUrb;
                                            (*user_urb_ptr).status = 0;
                                            (*user_urb_ptr).actual_length =
                                                data_buffer.len() as i32;
                                        }

                                        // 设置URB为成功状态
                                        req.status = 0;
                                        req.actual_length = data_buffer.len() as i32;

                                        // 将URB放入已完成队列
                                        self.completed_urbs.lock().push((arg, req));

                                        return Ok(0);
                                    }
                                    (0x81, USB_DIR_IN) | (0x83, USB_DIR_IN) => {
                                        warn!("UVC: GET_{:0x} 请求", setup.bRequest);

                                        let response = generate_default_probe_control();
                                        debug_probe_control(&response);

                                        // 计算数据在URB缓冲区中的位置（在setup包之后）
                                        let data_offset = core::mem::size_of::<UsbControlSetup>();
                                        let data_ptr = unsafe { req.buffer.add(data_offset) };

                                        // 将状态数据拷贝到用户空间
                                        copy_to_user(data_ptr, response.as_ptr(), response.len())?;

                                        // 更新用户空间的 status 和 actual_length
                                        unsafe {
                                            let user_urb_ptr = arg as *mut UsbdevfsUrb;
                                            (*user_urb_ptr).status = 0;
                                            (*user_urb_ptr).actual_length = response.len() as i32;
                                        }

                                        // 设置URB为成功状态
                                        req.status = 0;
                                        req.actual_length = response.len() as i32;

                                        // 将URB放入已完成队列
                                        self.completed_urbs.lock().push((arg, req));
                                        return Ok(0);
                                    }
                                    _ => {
                                        warn!("不支持的UVC请求: {}", setup.bRequest);
                                    }
                                },
                                _ => {
                                    warn!("未知请求类型: 0x{:02x}", request_type);
                                }
                            }
                        }
                        _ => {
                            warn!("unkonw urb_type: {}", req.urb_type);
                        }
                    }
                    return Err(VfsError::Unsupported);
                }
                USBDEVFS_REAPURBNDELAY_NR => {
                    warn!("USBDEVFS_REAPURBNDELAY_NR");

                    let mut completed_urbs = self.completed_urbs.lock();
                    if let Some((pre_arg, urb)) = completed_urbs.pop() {
                        warn!(
                            "返回已完成的URB: type={}, status={}, actual_length={}",
                            urb.urb_type, urb.status, urb.actual_length
                        );
                        copy_to_user(
                            pre_arg as *mut u8,
                            &urb as *const _ as *const u8,
                            core::mem::size_of::<UsbdevfsUrb>(),
                        )?;
                        let src_ptr = &pre_arg as *const usize as *const u8;
                        copy_to_user(arg as *mut u8, src_ptr, core::mem::size_of::<usize>())?;
                        return Ok(0);
                    }

                    let mut waiting_urbs = self.waiting_urbs.lock();
                    if waiting_urbs.is_empty() {
                        warn!("USBDEVFS_REAPURBNDELAY_NR wait");
                        return Err(VfsError::WouldBlock);
                    }

                    let (pre_arg, mut urb, mut iso_descs, iso_descs_user_ptr) =
                        waiting_urbs.pop().unwrap();
                    warn!(
                        "ISO URB number_of_packets: {}, buffer_length: {}, iso_descs size: {}",
                        urb.number_of_packets,
                        urb.buffer_length,
                        iso_descs.len(),
                    );

                    let mut cur_frame_event = self.cur_frame_event.lock();
                    let mut cur_frame_data_offset = self.cur_frame_data_offset.lock();
                    let mut fid = *self.cur_fid.lock();
                    if cur_frame_event.is_none() {
                        let mut frame_events = self.frame_events.lock();
                        if frame_events.len() <= 0 {
                            warn!("wait next frame_event");
                            return Err(VfsError::WouldBlock);
                        }
                        warn!("new frame_event");
                        *cur_frame_event = frame_events.pop();
                        *cur_frame_data_offset = 0;

                        // 切换 fid
                        fid ^= 1;
                        *self.cur_fid.lock() = fid;
                    } else {
                        warn!("use pre frame_event");
                    }

                    let frame_event = cur_frame_event.as_ref().unwrap();

                    // 计算总数据量和每个包的分配
                    let total_data_length = frame_event.data.len();
                    let mut data_offset = *cur_frame_data_offset;
                    let mut total_copied = 0;

                    for (i, desc) in iso_descs.iter_mut().enumerate() {
                        // 计算这个包应该复制多少数据
                        let remaining_data_len = total_data_length - data_offset;
                        // 确定实际复制长度 需要减去包头的长度
                        let data_copy_len = remaining_data_len.min(desc.length as usize - 2);
                        warn!(
                            "  ISO URB {} remaining_data: {}, data_copy_len: {}, desc length: {}, \
                             total_copied: {}",
                            i, remaining_data_len, data_copy_len, desc.length, total_copied
                        );

                        // 更新等时包描述符
                        desc.actual_length = desc.length;
                        desc.status = 0; // 成功

                        let is_eof = (data_offset + data_copy_len) >= total_data_length;
                        warn!("   is_eof: {}", is_eof);
                        let fid_bit = (fid & 0x01) << 0;
                        let eof_bit = if is_eof { 1 << 1 } else { 0 };
                        let eoh_bit = 1 << 7;
                        let header = UvcHeader {
                            header_length: 2,
                            header_info: fid_bit | eof_bit | eoh_bit,
                        };

                        let mut ikbuf = vec![0u8; desc.length as usize];
                        // 复制2字节UVC头部
                        warn!("  copy to kbuf header: {:?}", header);
                        ikbuf[0..2].copy_from_slice(unsafe {
                            core::slice::from_raw_parts(&header as *const _ as *const u8, 2)
                        });

                        // 复制数据到用户空间
                        warn!("  copy to user data_copy_len: {}", data_copy_len);
                        if data_copy_len > 0 {
                            ikbuf[2..(2 + data_copy_len)].copy_from_slice(
                                &frame_event.data[data_offset..(data_offset + data_copy_len)],
                            );
                        }

                        copy_to_user(urb.buffer, ikbuf.as_ptr(), ikbuf.len() as usize)?;

                        data_offset += data_copy_len;
                        total_copied += desc.length as usize;

                        warn!(
                            "  ISO URB {} length: {}, actual_length: {}",
                            i, desc.length, desc.actual_length
                        );

                        // 一次只回复一个数据包
                        break;
                    }

                    warn!(
                        "ISO URB total_copied: {}, urb.buffer_length: {}",
                        total_copied, urb.buffer_length
                    );

                    // 更新帧偏移量
                    *cur_frame_data_offset = data_offset;
                    // 复制完全
                    if total_data_length <= data_offset {
                        // 置空当前帧
                        *cur_frame_event = None;
                        warn!("cur_frame_event copied full")
                    }

                    // 更新URB状态
                    urb.status = 0; // 成功
                    urb.actual_length = total_copied as i32;

                    let iso_descs_size = (urb.number_of_packets as usize)
                        * core::mem::size_of::<IsoPacketDescriptor>();

                    // 将等时包描述符写回用户空间
                    warn!("copy to user iso descs");
                    copy_to_user(
                        iso_descs_user_ptr as *mut u8,
                        iso_descs.as_ptr() as *const u8,
                        iso_descs_size,
                    )?;

                    warn!("copy to user urb");
                    copy_to_user(
                        pre_arg as *mut u8,
                        &urb as *const _ as *const u8,
                        core::mem::size_of::<UsbdevfsUrb>(),
                    )?;

                    // 返回URB指针
                    warn!("copy to user urb ptr");
                    let src_ptr = &pre_arg as *const usize as *const u8;
                    copy_to_user(arg as *mut u8, src_ptr, core::mem::size_of::<usize>())?;

                    return Ok(0);
                }
                USBDEVFS_DISCARDURB_NR => {
                    warn!("USBDEVFS_DISCARDURB_NR");

                    // 读取用户要丢弃的 URB
                    let mut req = UsbdevfsUrb::default();
                    copy_from_user(
                        &mut req as *mut _ as *mut u8,
                        arg as *const u8,
                        core::mem::size_of::<UsbdevfsUrb>(),
                    )?;

                    let mut queue_urb = self.completed_urbs.lock();
                    for (i, (_, urb)) in queue_urb.iter().enumerate() {
                        if urb.buffer == req.buffer {
                            warn!("从完成队列中移除 urb 索引: {}", i);
                            queue_urb.remove(i);
                            break;
                        }
                    }

                    let mut queue_urb = self.waiting_urbs.lock();
                    for (i, (_, urb, ..)) in queue_urb.iter().enumerate() {
                        if urb.buffer == req.buffer {
                            warn!("从等待队列中移除 urb 索引: {}", i);
                            queue_urb.remove(i);
                            break;
                        }
                    }

                    Ok(0)
                }
                USBDEVFS_RESET_NR => {
                    warn!("USBDEVFS_RESET_NR");
                    Ok(0)
                }
                _ => {
                    warn!("unkonw U ioctl: {cmd}, {arg}");
                    Err(VfsError::InvalidInput)
                }
            },
            'V' => match ioc_nr {
                _ => {
                    warn!("unkonw V ioctl: {cmd}, {arg}");
                    Err(VfsError::InvalidInput)
                }
            },
            _ => {
                warn!("unkonw ioctl: {cmd}, {arg}");
                Err(VfsError::InvalidInput)
            }
        }
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

fn stream_loop(stream: &mut VideoStream, frame_events: Arc<Mutex<Vec<FrameEvent>>>) {
    block_on(async {
        let mut i = 0;
        loop {
            i += 1;
            let stream_frame_events = stream.recv().await.unwrap();
            
            if stream_frame_events.len() == 0 {
                continue;
            }
            
            let mut frame_events_guard = frame_events.lock();
            for stream_frame_event in stream_frame_events {
                // 如果缓冲池满了，踢掉最老的一帧，保证用户态拿到的是最新画面
                if frame_events_guard.len() >= frame_events_guard.capacity() {
                    frame_events_guard.remove(0); // 改为 remove(0) 丢弃旧数据，原代码的 pop 会丢弃新数据
                }
                frame_events_guard.push(stream_frame_event);
            }
            
            // 安全的调试信息：每 30 帧打印一次，不刷屏，不写磁盘！
            if i % 30 == 0 {
                let last_frame_size = frame_events_guard.last().map(|f| f.data.len()).unwrap_or(0);
                warn!("[UVC] 视频流正常接收中... 已抓取 {} 帧, 最新一帧大小: {} 字节", i, last_frame_size);
            }
        }
    });
}

/// Copies data from user space to kernel space
pub fn copy_from_user(dst: *mut u8, src: *const u8, size: usize) -> Result<(), axio::Error> {
    let ret = unsafe { user_copy(dst, src, size) };

    if ret != 0 {
        warn!("[rknpu]: copy_from_user failed, ret={}", ret);
        return Err(VfsError::InvalidData);
    }
    Ok(())
}

/// Copies data from kernel space to user space
pub fn copy_to_user(dst: *mut u8, src: *const u8, size: usize) -> Result<(), axio::Error> {
    let ret = unsafe { user_copy(dst, src, size) };

    if ret != 0 {
        warn!("[rknpu]: copy_to_user failed, ret={}", ret);
        return Err(VfsError::InvalidData);
    }
    Ok(())
}

// USBDEVFS ioctl nr
const USBDEVFS_GET_CAPABILITIES_NR: u32 = 26;
const USBDEVFS_GET_DRIVER_NR: u32 = 8;
const USBDEVFS_CLAIMINTERFACE_NR: u32 = 18;
const USBDEVFS_RELEASEINTERFACE_NR: u32 = 16;
const USBDEVFS_RESETEP_NR: u32 = 15;
const USBDEVFS_SUBMITURB_NR: u32 = 10;
const USBDEVFS_REAPURBNDELAY_NR: u32 = 13;
const USBDEVFS_DISCARDURB_NR: u32 = 11;
const USBDEVFS_RESET_NR: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct UsbdevfsCapabilities {
    capabilities: u32,
    reserved: [u32; 15],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct UsbdevfsDriver {
    interface: u32,
    driver: [u8; 255],
}

// USB 请求块 (URB) 结构体
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct UsbdevfsUrb {
    urb_type: u8,           // URB 类型
    endpoint: u8,           // 端点号
    status: i32,            // 状态
    flags: u32,             // 标志
    buffer: *mut u8,        // 数据缓冲区
    buffer_length: i32,     // 缓冲区长度
    actual_length: i32,     // 实际传输长度
    start_frame: i32,       // 起始帧号
    number_of_packets: i32, // 包数量
    error_count: i32,       // 错误计数
    signr: u32,             // 信号号
    usercontext: *mut u8,   // 用户上下文
}

unsafe impl Send for UsbdevfsUrb {}
unsafe impl Sync for UsbdevfsUrb {}

// 等时传输包描述符
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct IsoPacketDescriptor {
    length: u32,        // 包长度
    actual_length: u32, // 实际长度
    status: u32,        // 状态
}

// UVC头部结构
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct UvcHeader {
    header_length: u8, // 头部长度 (0x0C或0x02)
    header_info: u8,   // 头部信息 (FID, EOF等)
}

// URB 类型常量
const USBDEVFS_URB_TYPE_ISO: u8 = 0; // 等时传输
const USBDEVFS_URB_TYPE_CONTROL: u8 = 2; // 控制传输

// 控制请求
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct UsbControlSetup {
    bmRequestType: u8,
    bRequest: u8,
    wValue: u16,
    wIndex: u16,
    wLength: u16,
}

// 控制请求类型
const USB_TYPE_STANDARD: u8 = 0x00;
const USB_TYPE_CLASS: u8 = 0x20;

// USB 标准请求
const USB_REQ_GET_DESCRIPTOR: u8 = 6;

// 描述符类型
const USB_DT_STRING: u8 = 0x03;

const USB_TYPE_MASK: u8 = 0x60;

// 控制请求方向
const USB_DIR_OUT: u8 = 0x00;
const USB_DIR_IN: u8 = 0x80;

fn generate_default_probe_control() -> [u8; 26] {
    [
        0x01, 0x00, // bmHint
        0x01, // bFormatIndex
        0x01, // bFrameIndex
        0x15, 0x16, 0x05, 0x00, // dwFrameInterval (333333 - 30fps)
        0x00, 0x00, // wKeyFrameRate
        0x00, 0x00, // wPFrameRate
        0x00, 0x00, // wCompQuality
        0x00, 0x00, // wCompWindowSize
        0x00, 0x00, // wDelay
        0x00, 0x96, 0x00, 0x00, // dwMaxVideoFrameSize = 38400 (0x9600)
        0x00, 0x0C, 0x00, 0x00, // dwMaxPayloadTransferSize = 3072 (0x0C00)
    ]
}

fn debug_probe_control(data: &[u8]) {
    if data.len() < 26 {
        warn!("数据长度不足 26/{}", data.len());
        return;
    }

    let bm_hint = u16::from_le_bytes([data[0], data[1]]);
    let b_format_index = data[2];
    let b_frame_index = data[3];
    let dw_frame_interval = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let w_key_frame_rate = u16::from_le_bytes([data[8], data[9]]);
    let w_p_frame_rate = u16::from_le_bytes([data[10], data[11]]);
    let w_comp_quality = u16::from_le_bytes([data[12], data[13]]);
    let w_comp_window_size = u16::from_le_bytes([data[14], data[15]]);
    let w_delay = u16::from_le_bytes([data[16], data[17]]);
    let dw_max_video_frame_size = u32::from_le_bytes([data[18], data[19], data[20], data[21]]);
    // let dw_max_payload_transfer_size = u32::from_le_bytes([data[22], data[23], data[24], data[25]]);
    let dw_max_payload_transfer_size = u32::from_le_bytes(data[22..26].try_into().unwrap());

    warn!(
        "VS_PROBE_CONTROL:
    bmHint: 0x{:04X}
    bFormatIndex: {}
    bFrameIndex: {}
    dwFrameInterval: {}
    wKeyFrameRate: {}
    wPFrameRate: {}
    wCompQuality: {}
    wCompWindowSize: {}
    wDelay: {}
    dwMaxVideoFrameSize: {}
    dwMaxPayloadTransferSize: {}",
        bm_hint,
        b_format_index,
        b_frame_index,
        dw_frame_interval,
        w_key_frame_rate,
        w_p_frame_rate,
        w_comp_quality,
        w_comp_window_size,
        w_delay,
        dw_max_video_frame_size,
        dw_max_payload_transfer_size,
    );
}
