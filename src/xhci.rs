extern crate alloc;
use alloc::{
    boxed::Box,
    collections::VecDeque,
    rc::{Rc, Weak},
    string::String,
    string::ToString,
    vec,
    vec::Vec,
};
use core::sync::atomic::{fence, Ordering};

use core::{
    alloc::Layout,
    cmp::max,
    future::Future,
    marker::PhantomPinned,
    mem::transmute,
    mem::{size_of, MaybeUninit},
    ops::Range,
    pin::Pin,
    ptr::read_volatile,
    ptr::write_volatile,
    slice,
    task::{Context, Poll},
};

use crate::{
    allocator::ALLOCATOR,
    bits::extract_bits,
    cbw::CommandBlockWrapper,
    executor::spawn_global,
    executor::yield_execution,
    info,
    mmio::IoBox,
    mmio::Mmio,
    mutex::Mutex,
    pci::{BarMem64, BusDeviceFunction, Pci, VendorDeviceId},
    pin::IntoPinnedMutableSlice,
    result::Result,
    slice::Sliceable,
    volatile::Volatile,
    x86::busy_loop_hint,
};

struct XhcRegisters {
    cap_regs: Mmio<CapabilityRegisters>,
    op_regs: Mmio<OperationalRegisters>,
    rt_regs: Mmio<RuntimeRegisters>,
    doorbell_regs: Vec<Rc<Doorbell>>,
    portsc: PortSc,
}

pub struct PciXhciDriver {}
impl PciXhciDriver {
    pub fn supports(vd: VendorDeviceId) -> bool {
        const VDI_LIST: [VendorDeviceId; 4] = [
            VendorDeviceId {
                vendor: 0x1b36,
                device: 0x000d,
            },
            VendorDeviceId {
                vendor: 0x8086,
                device: 0x31a8,
            },
            VendorDeviceId {
                vendor: 0x8086,
                device: 0x02ed,
            },
            VendorDeviceId {
                vendor: 0x8086,
                device: 0x9d2f,
            }, // 100 Series/C230 Series Chipset Family USB 3.0 xHCI Controller
        ];
        VDI_LIST.contains(&vd)
    }
    fn setup_xhc_registers(bar0: &BarMem64) -> Result<(XhcRegisters)> {
        let cap_regs = unsafe { Mmio::from_raw(bar0.addr() as *mut CapabilityRegisters) };
        let op_regs = unsafe {
            Mmio::from_raw(
                bar0.addr().add(cap_regs.as_ref().caplength()) as *mut OperationalRegisters
            )
        };
        let rt_regs = unsafe {
            Mmio::from_raw(bar0.addr().add(cap_regs.as_ref().rtsoff()) as *mut RuntimeRegisters)
        };
        let portsc = PortSc::new(bar0, cap_regs.as_ref());
        let num_slots = cap_regs.as_ref().num_of_ports();
        let mut doorbell_regs = Vec::new();
        for i in 0..=num_slots {
            let ptr = unsafe { bar0.addr().add(cap_regs.as_ref().dboff()).add(4 * i) as *mut u32 };
            doorbell_regs.push(Rc::new(Doorbell::new(ptr)));
        }
        assert!(doorbell_regs.len() == 1 + num_slots);
        Ok(XhcRegisters {
            cap_regs,
            op_regs,
            rt_regs,
            portsc,
            doorbell_regs,
        })
    }
    pub fn attach(pci: &Pci, bdf: BusDeviceFunction) -> Result<()> {
        info!("Xhci found at: {bdf:?}");
        pci.disable_interrupt(bdf)?;
        pci.enable_bus_mastering(bdf)?;
        let bar0 = pci.try_bar0_mem64(bdf)?;
        bar0.disable_cach();
        let regs = Self::setup_xhc_registers(&bar0)?;
        let xhc = Controller::new(regs)?;
        spawn_global(Self::run(xhc));
        Ok(())
    }
    async fn run(xhc: Controller) -> Result<()> {
        /*
                info!(
                    "xHCI  cap_regs.MaxSlots = {}",
                    xhc.regs.cap_regs.as_ref().num_of_device_slots()
                );
                info!(
                    "xHCI  op_regs.USBSTS = {:#x}",
                    xhc.regs.op_regs.as_ref().usbsts()
                );
                info!(
                    "xHCI  rt_regs.MFINDEX = {:#x}",
                    xhc.regs.rt_regs.as_ref().mfindex()
                );
                info!("portsc values for port {:?}", xhc.regs.portsc.port_range());
        */
        let mut connected_port: Vec<usize> = Vec::new();

        for port in xhc.regs.portsc.port_range() {
            if let Some(e) = xhc.regs.portsc.get(port) {
                //info!("{port:3}: {:#010X}", e.value());
                if e.ccs() {
                    connected_port.push(port);
                }
            }
        }
        let xhc = Rc::new(xhc);
        {
            let xhc = xhc.clone();
            spawn_global(async move {
                loop {
                    // とりあえずポーリングは動く
                    match xhc.primary_event_ring.lock().poll().await {
                        Ok(_) => {}
                        Err(e) => {
                            info!("Event polling error: {:?}", e);
                        }
                    }
                    yield_execution().await;
                }
            })
        }
        let xhc_clone = xhc.clone();
        for port in connected_port.iter() {
            //info!("xHCI: port {port} is connected ");
            let slot = Self::init_port(&xhc_clone, *port).await?;
            info!("xHCI: slot {slot} is assigned for port {port}");
            let ctrl_ep_ring = Self::address_device(&xhc_clone, *port, slot).await;

            if let Err(e) = &ctrl_ep_ring {
                info!("Failed to address device on port {port}: {:?}", e);
                continue;
            }

            let mut ctrl_ep_ring = ctrl_ep_ring?;
            let mut device_descriptor =
                Self::request_device_descriptor(&xhc_clone, slot, &mut ctrl_ep_ring).await;
            if let Err(e) = &device_descriptor {
                info!("Failed to address device on port {port}: {:?}", e);
                continue;
            } else {
                device_descriptor = Ok(*device_descriptor.as_ref().unwrap());
                let device_descriptor = device_descriptor.unwrap();
                let vid = device_descriptor.vendor_id;
                let pid = device_descriptor.product_id;
                info!("xHCI: Device VID: {vid:#06X}, PID: {pid:#06X}");
                if let Ok(e) =
                    Self::request_string_descriptor_zero(&xhc, slot, &mut ctrl_ep_ring).await
                {
                    let lang_id = e[1];
                    let vendor = if device_descriptor.manufacturer_index != 0 {
                        Some(
                            Self::request_string_descriptor(
                                &xhc,
                                slot,
                                &mut ctrl_ep_ring,
                                lang_id,
                                device_descriptor.manufacturer_index,
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    let product = if device_descriptor.product_index != 0 {
                        Some(
                            Self::request_string_descriptor(
                                &xhc,
                                slot,
                                &mut ctrl_ep_ring,
                                lang_id,
                                device_descriptor.product_index,
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    let serial = if device_descriptor.serial_idx != 0 {
                        Some(
                            Self::request_string_descriptor(
                                &xhc,
                                slot,
                                &mut ctrl_ep_ring,
                                lang_id,
                                device_descriptor.serial_idx,
                            )
                            .await?,
                        )
                    } else {
                        None
                    };

                    info!("xHCI: V/P/S: {:?}/{:?}/{:?}", vendor, product, serial);
                    let descriptors =
                        Self::request_config_descriptor_and_rest(&xhc, slot, &mut ctrl_ep_ring)
                            .await?;

                    for desc in &descriptors {
                        match desc {
                            UsbDescriptor::Interface(interface) => {
                                if interface.interface_class == 8 && // Mass Storage Class
                                           interface.interface_subclass == 6 && // SCSI transparent command set
                                           interface.interface_protocol == 0x50
                                {
                                    // Bulk-Only Transport
                                    Self::handle_mass_storage_device(
                                        &xhc,
                                        slot,
                                        &mut ctrl_ep_ring,
                                        &descriptors,
                                    )
                                    .await?;
                                }
                            }
                            _ => {}
                        }
                    }

                    /*
                    match &descriptors[1] {
                        UsbDescriptor::Config(config) => {
                            info!(
                                "xHCI: Config descriptor - total_length: {}, config_value: {}",
                                config.total_length(),
                                config.config_value()
                            );
                        }
                        UsbDescriptor::Interface(interface) => {
                            info!(
                                "xHCI: Interface descriptor: {},  {}",
                                interface.interface_class, interface.interface_subclass
                            );
                        }
                        UsbDescriptor::Endpoint(endpoint) => {
                            info!("xHCI: Endpoint descriptor: {:?}", endpoint);
                        }
                        UsbDescriptor::Unknown {
                            desc_len,
                            desc_type,
                        } => {
                            info!(
                                "xHCI: Unknown descriptor - len: {}, type: {}",
                                desc_len, desc_type
                            );
                        }
                    }*/
                }
            }
        }
        Ok(())
    }
    // Bulk-Only Mass Storage Reset実装
    async fn bulk_only_mass_storage_reset(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
    ) -> Result<()> {
        // Setup Stage: Bulk-Only Mass Storage Reset
        ctrl_ep_ring.push(
            SetupStageTrb::new(
                SetupStageTrb::REQ_TYPE_DIR_HOST_TO_DEVICE
                    | SetupStageTrb::REQ_TYPE_TYPE_CLASS
                    | SetupStageTrb::REQ_TYPE_TO_INTERFACE,
                0xFF, // Bulk-Only Mass Storage Reset
                0,    // wValue
                0,    // wIndex (Interface number)
                0,    // wLength
            )
            .into(),
        )?;

        // Status Stage
        let trb_ptr = ctrl_ep_ring.push(StatusStageTrb::new_in().into())?;

        xhc.notify_ep(slot, 1)?;
        EventFuture::new_for_trb(&xhc.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;

        Ok(())
    }
    // Mass Storageデバイスの処理
    async fn handle_mass_storage_device(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
        descriptors: &[UsbDescriptor],
    ) -> Result<()> {
        // エンドポイントを探す
        let mut bulk_in_ep = None;
        let mut bulk_out_ep = None;
        let mut last_config: Option<ConfigDescriptor> = None;
        let mut usb_interface: Option<InterfaceDescriptor> = None;
        let mut ep_desc_list: Vec<EndpointDescriptor> = Vec::new();

        for desc in descriptors {
            match desc {
                UsbDescriptor::Config(ep) => {
                    if usb_interface.is_some() {
                        break;
                    }
                    last_config = Some(*ep);
                    ep_desc_list.clear();
                }
                UsbDescriptor::Interface(e) => {
                    usb_interface = Some(*e);
                }
                UsbDescriptor::Endpoint(ep) => {
                    if ep.attributes & 0x03 == 2 {
                        // Bulk transfer
                        if ep.endpoint_address & 0x80 != 0 {
                            // IN endpoint
                            bulk_in_ep = Some(ep);
                        } else {
                            // OUT endpoint
                            bulk_out_ep = Some(ep);
                        }
                    }
                    ep_desc_list.push(*ep);
                }
                _ => {}
            }
        }
        let config_desc = last_config.ok_or("Config descriptor not found")?;
        let interface_desc = usb_interface.ok_or("Interface descriptor not found")?;
        let bulk_in = bulk_in_ep.ok_or("Bulk IN endpoint not found")?;
        let bulk_out = bulk_out_ep.ok_or("Bulk OUT endpoint not found")?;
        info!(
            "xHCI: Found bulk endpoints - IN: {:#x}, OUT: {:#x}",
            bulk_in.endpoint_address, bulk_out.endpoint_address
        );

        // Configuration を設定
        xhc.request_set_config(slot, ctrl_ep_ring, config_desc.config_value())
            .await?;

        Self::bulk_only_mass_storage_reset(xhc, slot, ctrl_ep_ring).await?;

        // エンドポイント----------------------------------------------------------------
        let bulk_out_dci = if bulk_out.endpoint_address & 0x80 == 0 {
            // OUT エンドポイント: EP番号 * 2
            let ep_num = bulk_out.endpoint_address & 0x0F;
            (ep_num * 2) as usize
        } else {
            return Err("OUT endpoint has IN direction bit set");
        };

        let bulk_in_dci = if bulk_in.endpoint_address & 0x80 != 0 {
            // IN エンドポイント: EP番号 * 2 + 1
            let ep_num = bulk_in.endpoint_address & 0x0F;
            (ep_num * 2 + 1) as usize
        } else {
            return Err("IN endpoint has OUT direction bit set");
        };
        // ------------------------------------------------------------------------

        let mut bulk_out_ring =
            Self::configure_bulk_endpoint(xhc, slot, bulk_out, bulk_out_dci).await?;

        let mut bulk_in_ring =
            Self::configure_bulk_endpoint(xhc, slot, bulk_in, bulk_in_dci).await?;

        // Bulk エンドポイント設定後にSTALLをクリア
        if let Some(bulk_in_ep) = bulk_in_ep {
            xhc.clear_endpoint_halt(slot, ctrl_ep_ring, bulk_in_ep.endpoint_address)
                .await
                .unwrap_or_else(|e| {
                    info!("Failed to clear bulk IN endpoint halt: {:?}", e);
                });
        }

        if let Some(bulk_out_ep) = bulk_out_ep {
            xhc.clear_endpoint_halt(slot, ctrl_ep_ring, bulk_out_ep.endpoint_address)
                .await
                .unwrap_or_else(|e| {
                    info!("Failed to clear bulk OUT endpoint halt: {:?}", e);
                });
        }
        Self::scsi_read_10(
            xhc,
            slot,
            &mut bulk_in_ring,
            &mut bulk_out_ring,
            bulk_in_dci,
            bulk_out_dci,
            0,
            1,
        )
        .await?;

        Ok(())
    }

    async fn scsi_inquiry(
        xhc: &Rc<Controller>,
        slot: u8,
        bulk_in_ring: &mut CommandRing,
        bulk_out_ring: &mut CommandRing,
        bulk_in_dci: usize,
        bulk_out_dci: usize,
    ) -> Result<Vec<u8>> {
        info!("xHCI: Starting SCSI INQUIRY command");

        // CBW作成 (31バイト) - INQUIRY用
        let mut cbw_data = vec![0u8; 31];
        cbw_data[0..4].copy_from_slice(&0x43425355u32.to_le_bytes()); // "USBC"
        cbw_data[4..8].copy_from_slice(&1u32.to_le_bytes()); // CBWタグ
        cbw_data[8..12].copy_from_slice(&36u32.to_le_bytes()); // データ転送長（36バイト）
        cbw_data[12] = 0x80; // bmCBWFlags (Data-In)
        cbw_data[13] = 0; // bCBWLUN
        cbw_data[14] = 6; // bCBWCBLength (INQUIRYコマンド長)

        // SCSI INQUIRYコマンド
        cbw_data[15] = 0x12; // INQUIRY オペレーションコード
        cbw_data[16] = 0x00; // EVPD=0
        cbw_data[17] = 0x00; // Page Code
        cbw_data[18] = 0x00; // Reserved
        cbw_data[19] = 36; // Allocation Length
        cbw_data[20] = 0x00; // Control

        let mut cbw_data = Box::into_pin(cbw_data.into_boxed_slice());

        // CBW送信
        let trb_ptr = bulk_out_ring.push(NormalTrb::new_out(cbw_data.as_mut()).into())?;
        xhc.notify_ep(slot, bulk_out_dci)?;
        EventFuture::new_for_trb(&xhc.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;
        info!("xHCI: INQUIRY CBW sent successfully");

        // データ受信（36バイト）
        let read_data = vec![0u8; 36];
        let mut read_data = Box::into_pin(read_data.into_boxed_slice());

        let trb_ptr = bulk_in_ring.push(NormalTrb::new_in(read_data.as_mut()).into())?;
        xhc.notify_ep(slot, bulk_in_dci)?;
        EventFuture::new_for_trb(&xhc.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;

        // CSW受信
        let csw = vec![0u8; 13];
        let mut csw = Box::into_pin(csw.into_boxed_slice());

        let trb_ptr = bulk_in_ring.push(NormalTrb::new_in(csw.as_mut()).into())?;
        xhc.notify_ep(slot, bulk_in_dci)?;
        EventFuture::new_for_trb(&xhc.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;

        info!("xHCI: INQUIRY data: {:02x?}", &read_data[..]);
        Ok(read_data.to_vec())
    }

    // SCSI READ(10) コマンドの追加
    async fn scsi_read_10(
        xhc: &Rc<Controller>,
        slot: u8,
        bulk_in_ring: &mut CommandRing,
        bulk_out_ring: &mut CommandRing,
        bulk_in_dci: usize,
        bulk_out_dci: usize,
        lba: u32,
        transfer_length: u16,
    ) -> Result<Vec<u8>> {
        // CBW作成 (31バイト) - READ(10)用
        let mut cbw_data = vec![0u8; 31];
        cbw_data[0..4].copy_from_slice(&0x43425355u32.to_le_bytes()); // "USBC"
        cbw_data[4..8].copy_from_slice(&3u32.to_le_bytes()); // CBWタグ
        cbw_data[8..12].copy_from_slice(&((transfer_length as u32) * 512).to_le_bytes()); // データ転送長
        cbw_data[12] = 0x80; // bmCBWFlags (Data-In)
        cbw_data[13] = 0; // bCBWLUN
        cbw_data[14] = 10; // bCBWCBLength (READ(10)コマンド長)

        // SCSI READ(10)コマンド
        cbw_data[15] = 0x28; // READ(10) オペレーションコード
        cbw_data[16] = 0x00; // RelAdr=0, FUA=0, DPO=0
        cbw_data[17..21].copy_from_slice(&lba.to_be_bytes()); // LBA (ビッグエンディアン)
        cbw_data[21] = 0x00; // GroupNumber
        cbw_data[22..24].copy_from_slice(&transfer_length.to_be_bytes()); // TransferLength (ビッグエンディアン)
        cbw_data[24] = 0x00; // Control

        let mut cbw_data = Box::into_pin(cbw_data.into_boxed_slice());
        // CBW送信
        let cbw_trb = NormalTrb::new_out(cbw_data.as_mut());
        let cbw_trb_ptr = bulk_out_ring.push(cbw_trb.into())?;
        info!("CBW TRB pushed at address: {:#x}", cbw_trb_ptr);

        xhc.notify_ep(slot, bulk_out_dci)?;
        let usbsts = xhc.regs.op_regs.as_ref().usbsts();
        info!(
            "After Doorbell: USBSTS={:#x} => HCHalted={}, HostErr={}, EventInt={}, PortChg={}, HostCtrEvt={}",
            usbsts,
            (usbsts & 0x1) != 0,
            (usbsts & 0x2) != 0,
            (usbsts & 0x4) != 0,
            (usbsts & 0x8) != 0,
            (usbsts & 0x10) != 0,
        );
        info!("Waiting for CBW transfer event at {:#x}", cbw_trb_ptr);

        EventFuture::new_for_trb(&xhc.primary_event_ring, cbw_trb_ptr)
            .await?
            .transfer_result_ok()?;
        info!("xHCI: READ(10) CBW sent successfully");

        // データ受信
        let data_size = (transfer_length as usize) * 512;
        let read_data = vec![0u8; data_size];
        let mut read_data = Box::into_pin(read_data.into_boxed_slice());

        let data_trb_ptr = bulk_in_ring.push(NormalTrb::new_in(read_data.as_mut()).into())?;
        info!("Data TRB pushed at address: {:#x}", data_trb_ptr);

        xhc.notify_ep(slot, bulk_in_dci)?;
        let usbsts = xhc.regs.op_regs.as_ref().usbsts();
        info!(
            "After Doorbell: USBSTS={:#x} => HCHalted={}, HostErr={}, EventInt={}, PortChg={}, HostCtrEvt={}",
            usbsts,
            (usbsts & 0x1) != 0,
            (usbsts & 0x2) != 0,
            (usbsts & 0x4) != 0,
            (usbsts & 0x8) != 0,
            (usbsts & 0x10) != 0,
        );
        info!("Waiting for data transfer event at {:#x}", data_trb_ptr);

        EventFuture::new_for_trb(&xhc.primary_event_ring, data_trb_ptr)
            .await?
            .transfer_result_ok()?;
        info!("xHCI: Data received successfully");

        // CSW受信
        let csw = vec![0u8; 13];
        let mut csw = Box::into_pin(csw.into_boxed_slice());

        let csw_trb_ptr = bulk_in_ring.push(NormalTrb::new_in(csw.as_mut()).into())?;
        info!("CSW TRB pushed at address: {:#x}", csw_trb_ptr);

        xhc.notify_ep(slot, bulk_in_dci)?;
        let usbsts = xhc.regs.op_regs.as_ref().usbsts();
        info!(
            "After Doorbell: USBSTS={:#x} => HCHalted={}, HostErr={}, EventInt={}, PortChg={}, HostCtrEvt={}",
            usbsts,
            (usbsts & 0x1) != 0,
            (usbsts & 0x2) != 0,
            (usbsts & 0x4) != 0,
            (usbsts & 0x8) != 0,
            (usbsts & 0x10) != 0,
        );
        info!("Waiting for CSW transfer event at {:#x}", csw_trb_ptr);

        EventFuture::new_for_trb(&xhc.primary_event_ring, csw_trb_ptr)
            .await?
            .transfer_result_ok()?;
        info!("xHCI: CSW received successfully");

        // CSWの検証
        let csw_signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
        let csw_tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
        let csw_status = csw[12];

        info!(
            "CSW: signature={:#x}, tag={}, status={}",
            csw_signature, csw_tag, csw_status
        );

        if csw_signature != 0x53425355 {
            // "USBS"
            return Err("Invalid CSW signature");
        }

        if csw_status != 0 {
            return Err("SCSI command failed");
        }

        // データの最初の部分をログ出力
        Self::analyze_sector_zero(&read_data);
        Ok(read_data.to_vec())
    }

    // Bulk エンドポイントの設定
    async fn configure_bulk_endpoint(
        xhc: &Rc<Controller>,
        slot: u8,
        ep_desc: &EndpointDescriptor,
        dci: usize,
    ) -> Result<CommandRing> {
        let ep_ring = CommandRing::default();

        let mut input_ctrl_ctx = InputControlContext::default();
        input_ctrl_ctx.add_context(0)?; // Slot context
        input_ctrl_ctx.add_context(dci)?; // Endpoint context

        let mut input_context = Box::pin(InputContext::default());
        input_context.as_mut().set_input_ctrl_ctx(input_ctrl_ctx)?;

        let ep_type = if ep_desc.endpoint_address & 0x80 != 0 {
            EndpointType::BulkIn
        } else {
            EndpointType::BulkOut
        };

        let ep_ctx = EndpointContext::new_bulk_endpoint(
            ep_desc.max_packet_size,
            ep_ring.ring_phys_addr(),
            ep_type,
        )?;

        input_context.as_mut().set_ep_ctx(dci, ep_ctx)?;
        let current_max_dci = if dci > 1 { dci } else { 1 };
        input_context
            .as_mut()
            .set_last_valid_dci(dci.max(current_max_dci))?; // 最大DCIを更新

        let cmd = GenericTrbEntry::cmd_configure_endpoint(input_context.as_ref(), slot);

        let result = xhc.send_command(cmd).await;

        match &result {
            Ok(_) => info!("Configure Endpoint command succeeded"),
            Err(e) => info!("Configure Endpoint command failed: {:?}", e),
        }
        result?.cmd_result_ok()?;

        Ok(ep_ring)
    }

    // READ CAPACITY応答の解析
    fn parse_read_capacity_response(data: &[u8]) -> Result<(u32, u32)> {
        if data.len() < 8 {
            return Err("READ CAPACITY response too short");
        }

        // READ CAPACITY(10)レスポンス構造：
        // [0-3]: 最後のLBA (ビッグエンディアン)
        // [4-7]: ブロックサイズ (ビッグエンディアン)
        let last_lba = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

        let total_blocks = last_lba + 1; // LBAは0ベースなので+1
        let total_size_mb = (total_blocks as u64 * block_size as u64) / (1024 * 1024);

        info!("xHCI: READ CAPACITY Response:");
        info!("  Last LBA: {} (0x{:08X})", last_lba, last_lba);
        info!("  Block Size: {} bytes", block_size);
        info!("  Total Blocks: {}", total_blocks);
        info!("  Total Size: {} MB", total_size_mb);
        info!("  Raw data: {:02x?}", data);

        Ok((last_lba, block_size))
    }
    async fn init_port(xhc: &Rc<Controller>, port: usize) -> Result<u8> {
        let portsc = xhc.regs.portsc.get(port).ok_or("Port not found")?;
        //info!("xHCI: resetting port {port}");
        portsc.reset_port().await;
        //info!("xHCI: port {port} has been reset");
        portsc
            .is_enabled()
            .then_some(())
            .ok_or("Port is not enabled")?;
        //info!("xHCI: port {port} is enabled");
        let slot = xhc
            .send_command(GenericTrbEntry::cmd_enable_slot())
            .await?
            .slot_id();
        Ok(slot)
    }
    async fn address_device(xhc: &Rc<Controller>, port: usize, slot: u8) -> Result<CommandRing> {
        let output_context = Box::pin(OutputContext::default());
        xhc.set_output_context_for_slot(slot, output_context);
        let mut input_ctrl_ctx = InputControlContext::default();
        input_ctrl_ctx.add_context(0)?;
        input_ctrl_ctx.add_context(1)?;
        let mut input_context = Box::pin(InputContext::default());
        input_context.as_mut().set_input_ctrl_ctx(input_ctrl_ctx)?;
        input_context.as_mut().set_root_hub_port_number(port)?;
        input_context.as_mut().set_last_valid_dci(1)?;
        let portsc = xhc.regs.portsc.get(port).ok_or("PORTSC was invalid")?;
        input_context.as_mut().set_port_speed(portsc.port_speed())?;
        let ctrl_ep_ring = CommandRing::default();
        input_context.as_mut().set_ep_ctx(
            1,
            EndpointContext::new_control_endpoint(
                portsc.max_packet_size()?,
                ctrl_ep_ring.ring_phys_addr(),
            )?,
        )?;
        let cmd = GenericTrbEntry::cmd_address_device(input_context.as_ref(), slot);
        xhc.send_command(cmd).await?.cmd_result_ok()?;
        Ok(ctrl_ep_ring)
    }
    async fn request_device_descriptor(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
    ) -> Result<UsbDeviceDescriptor> {
        let mut desc = Box::pin(UsbDeviceDescriptor::default());
        xhc.request_descriptor(
            slot,
            ctrl_ep_ring,
            UsbDescriptorType::Device,
            0,
            0,
            desc.as_mut().as_mut_slice(),
        )
        .await?;
        Ok(*desc)
    }
    async fn request_string_descriptor(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
        lang_id: u16,
        index: u8,
    ) -> Result<String> {
        let buf = vec![0; 128];
        let mut buf = Box::into_pin(buf.into_boxed_slice());
        xhc.request_descriptor(
            slot,
            ctrl_ep_ring,
            UsbDescriptorType::String,
            index,
            lang_id,
            buf.as_mut(),
        )
        .await?;
        Ok(String::from_utf8_lossy(&buf[2..])
            .to_string()
            .replace('\0', ""))
    }
    async fn request_string_descriptor_zero(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
    ) -> Result<Vec<u16>> {
        let buf = vec![0; 8];
        let mut buf = Box::into_pin(buf.into_boxed_slice());
        xhc.request_descriptor(
            slot,
            ctrl_ep_ring,
            UsbDescriptorType::String,
            0,
            0,
            buf.as_mut(),
        )
        .await?;
        Ok(buf.as_ref().get_ref().to_vec())
    }
    async fn request_config_descriptor_and_rest(
        xhc: &Rc<Controller>,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
    ) -> Result<Vec<UsbDescriptor>> {
        let mut config_descriptor = Box::pin(ConfigDescriptor::default());
        xhc.request_descriptor(
            slot,
            ctrl_ep_ring,
            UsbDescriptorType::Config,
            0,
            0,
            config_descriptor.as_mut().as_mut_slice(),
        )
        .await?;
        let buf = vec![0; config_descriptor.total_length()];
        let mut buf = Box::into_pin(buf.into_boxed_slice());
        xhc.request_descriptor(
            slot,
            ctrl_ep_ring,
            UsbDescriptorType::Config,
            0,
            0,
            buf.as_mut(),
        )
        .await?;
        let iter = DescriptorIterator::new(&buf);
        let descriptors: Vec<UsbDescriptor> = iter.collect();
        Ok(descriptors)
    }
    // 新しい関数：セクタ0（MBR）の解析
    fn analyze_sector_zero(data: &[u8]) {
        info!("=== Master Boot Record (MBR) Analysis ===");

        // MBRの構造を解析
        if data.len() >= 512 {
            // ブートシグネチャをチェック（オフセット510-511に0x55AAがあるはず）
            let boot_signature = u16::from_le_bytes([data[510], data[511]]);
            info!(
                "Boot Signature: {:#x} {}",
                boot_signature,
                if boot_signature == 0xAA55 {
                    "(Valid MBR)"
                } else {
                    "(Invalid MBR)"
                }
            );

            // パーティションテーブル（オフセット446-509）の解析
            info!("=== Partition Table ===");
            for i in 0..4 {
                let offset = 446 + i * 16;
                if offset + 16 <= data.len() {
                    let partition = &data[offset..offset + 16];
                    Self::analyze_partition_entry(i + 1, partition);
                }
            }
            // ASCII文字列を探す
            Self::find_ascii_strings(data);
        }
    }

    // パーティションエントリの解析
    fn analyze_partition_entry(partition_num: usize, entry: &[u8]) {
        if entry.len() >= 16 {
            let boot_flag = entry[0];
            let start_head = entry[1];
            let start_sector = entry[2] & 0x3F;
            let start_cylinder = ((entry[2] & 0xC0) << 2) | entry[3];
            let partition_type = entry[4];
            let end_head = entry[5];
            let end_sector = entry[6] & 0x3F;
            let end_cylinder = ((entry[6] & 0xC0) << 2) | entry[7];
            let lba_start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
            let sector_count = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);

            info!(
                "Partition {}: Boot={:#x}, Type={:#x} ({}), LBA={}, Sectors={}",
                partition_num,
                boot_flag,
                partition_type,
                Self::partition_type_name(partition_type),
                lba_start,
                sector_count
            );

            if sector_count > 0 {
                let size_mb = (sector_count as u64 * 512) / (1024 * 1024);
                info!(
                    "  Size: {} MB, CHS Start: {}/{}/{}, CHS End: {}/{}/{}",
                    size_mb,
                    start_cylinder,
                    start_head,
                    start_sector,
                    end_cylinder,
                    end_head,
                    end_sector
                );
            }
        }
    }

    // パーティションタイプ名の取得
    fn partition_type_name(partition_type: u8) -> &'static str {
        match partition_type {
            0x00 => "Empty",
            0x01 => "FAT12",
            0x04 => "FAT16 <32MB",
            0x05 => "Extended",
            0x06 => "FAT16",
            0x07 => "NTFS/HPFS/exFAT",
            0x0B => "FAT32",
            0x0C => "FAT32 LBA",
            0x0E => "FAT16 LBA",
            0x0F => "Extended LBA",
            0x82 => "Linux Swap",
            0x83 => "Linux",
            0xEE => "GPT Protective",
            _ => "Unknown",
        }
    }

    // ASCII文字列を探す
    fn find_ascii_strings(data: &[u8]) {
        info!("=== ASCII Strings Found ===");
        let mut current_string = String::new();
        let mut start_offset = 0;

        for (i, &byte) in data.iter().enumerate() {
            if byte.is_ascii_graphic() || byte == b' ' {
                if current_string.is_empty() {
                    start_offset = i;
                }
                current_string.push(byte as char);
            } else {
                if current_string.len() >= 4 {
                    // 4文字以上の文字列のみ表示
                    info!("  Offset {:#x}: \"{}\"", start_offset, current_string);
                }
                current_string.clear();
            }
        }

        // 最後の文字列をチェック
        if current_string.len() >= 4 {
            info!("  Offset {:#x}: \"{}\"", start_offset, current_string);
        }
    }
}

#[repr(C)]
struct CapabilityRegisters {
    caplength: Volatile<u8>,
    reserved: Volatile<u8>,
    version: Volatile<u16>,
    hcsparams1: Volatile<u32>,
    hcsparams2: Volatile<u32>,
    hcsparams3: Volatile<u32>,
    hccparams1: Volatile<u32>,
    dboff: Volatile<u32>,
    rtsoff: Volatile<u32>,
    hccparams2: Volatile<u32>,
}
const _: () = assert!(size_of::<CapabilityRegisters>() == 0x20);
impl CapabilityRegisters {
    fn caplength(&self) -> usize {
        self.caplength.read() as usize
    }
    fn rtsoff(&self) -> usize {
        self.rtsoff.read() as usize
    }
    fn num_of_device_slots(&self) -> usize {
        extract_bits(self.hcsparams1.read(), 0, 8) as usize
    }
    fn num_scratchpad_bufs(&self) -> usize {
        (extract_bits(self.hcsparams2.read(), 21, 5) << 5
            | extract_bits(self.hcsparams2.read(), 27, 5)) as usize
    }
    fn num_of_ports(&self) -> usize {
        extract_bits(self.hcsparams1.read(), 24, 8) as usize
    }
    pub fn dboff(&self) -> usize {
        self.dboff.read() as usize
    }
}

#[repr(C, align(64))]
struct RawDeviceContextBaseAddressArray {
    scratchpad_table_ptr: *const *const u8,
    context: [u64; 255],
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<RawDeviceContextBaseAddressArray>() == 2048);
impl RawDeviceContextBaseAddressArray {
    fn new() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }
}
#[repr(C)]
struct OperationalRegisters {
    usbcmd: Volatile<u32>,
    usbsts: Volatile<u32>,
    pagesize: Volatile<u32>,
    rsvdz1: [u32; 2],
    dnctrl: Volatile<u32>,
    crcr: Volatile<u64>,
    rsvdz2: [u64; 2],
    dcbaap: Volatile<*const RawDeviceContextBaseAddressArray>,
    config: Volatile<u64>,
}
const _: () = assert!(size_of::<OperationalRegisters>() == 0x40);
impl OperationalRegisters {
    const STATUS_HC_HALTED: u32 = 0b0001;
    const CMD_RUN_STOP: u32 = 0b0001;
    const CMD_HC_RESET: u32 = 0b0010;
    fn usbsts(&self) -> u32 {
        self.usbsts.read()
    }
    fn page_size(&self) -> Result<usize> {
        let page_size_bits = self.pagesize.read() & 0xffff;
        if page_size_bits.count_ones() != 1 {
            return Err("Invalid page size bits");
        }
        let page_size_shift = page_size_bits.trailing_zeros();
        Ok(1 << (page_size_shift + 12))
    }
    fn reset_xhc(&mut self) {
        self.clear_command_bits(Self::CMD_RUN_STOP);
        while self.usbsts.read() & Self::STATUS_HC_HALTED == 0 {
            busy_loop_hint();
        }
        self.set_command_bits(Self::CMD_HC_RESET);
        while self.usbsts.read() & Self::CMD_HC_RESET != 0 {
            busy_loop_hint();
        }
    }
    fn start_xhc(&mut self) {
        self.set_command_bits(Self::CMD_RUN_STOP);
        while self.usbsts.read() & Self::STATUS_HC_HALTED != 0 {
            busy_loop_hint();
        }
    }
    fn set_cmd_ring_ctrl(&mut self, ring: &CommandRing) {
        self.crcr.write(ring.ring_phys_addr() | 1);
    }
    fn set_dcbaa_ptr(&mut self, dcbaa: &mut DeviceContextBaseAddressArray) -> Result<()> {
        self.dcbaap.write(dcbaa.inner_mut_ptr());
        Ok(())
    }
    fn set_num_device_slots(&mut self, num: usize) -> Result<()> {
        let c = self.config.read();
        let c = c & !0xff;
        let c = c | num as u64;
        self.config.write(c);
        Ok(())
    }
    fn set_command_bits(&mut self, bits: u32) {
        self.usbcmd.write(self.usbcmd.read() | bits);
    }
    fn clear_command_bits(&mut self, bits: u32) {
        self.usbcmd.write(self.usbcmd.read() & !bits);
    }
}

#[repr(C)]
struct InterrupterRegisterSet {
    management: u32,
    moderation: u32,
    erst_size: u32,
    rsvdp: u32,
    erst_base: u64,
    erdp: u64,
}
const _: () = assert!(size_of::<InterrupterRegisterSet>() == 0x20);

#[repr(C)]
struct RuntimeRegisters {
    mfindex: Volatile<u32>,
    rsvdz: [u32; 7],
    irs: [InterrupterRegisterSet; 1024],
}
const _: () = assert!(size_of::<RuntimeRegisters>() == 0x8020);
impl RuntimeRegisters {
    fn init_irs(&mut self, index: usize, ring: &mut EventRing) -> Result<()> {
        let irs = self.irs.get_mut(index).ok_or("Index out of range")?;
        // セグメント数
        irs.erst_size = 1;
        // ERST ベースアドレス
        irs.erst_base = ring.erst_phys_addr();
        // ERDP 初期値（リング先頭）
        irs.erdp = ring.ring_phys_addr();
        // Interrupt-Moderation = 0 → 即時に ERDP を更新
        irs.moderation = 0;
        // Interrupt Enable を立てる（Bit0=IE）
        irs.management = 1;
        // ソフトウェア側にも ERDP ポインタを教えておく
        ring.set_erdp(&mut irs.erdp as *mut u64);
        Ok(())
    }
}

struct ScratchpadBuffers {
    table: Pin<Box<[*const u8]>>,
    _bufs: Vec<Pin<Box<[u8]>>>,
}

impl ScratchpadBuffers {
    fn alloc(cap_regs: &CapabilityRegisters, op_regs: &OperationalRegisters) -> Result<Self> {
        let page_size = op_regs.page_size()?;
        info!("xHCI page size: {page_size}");
        let num_scratchpad_bufs = cap_regs.num_scratchpad_bufs();
        info!("xHCI: original num_scratchpad_bufs = {num_scratchpad_bufs}");

        let num_scratchpad_bufs = max(cap_regs.num_scratchpad_bufs(), 1);

        let table = ALLOCATOR.alloc_with_options(
            Layout::from_size_align(size_of::<usize>() * num_scratchpad_bufs, page_size)
                .map_err(|_| "Failed to allocate scratchpad buffer table")?,
        );
        let table = unsafe { slice::from_raw_parts(table as *mut *const u8, num_scratchpad_bufs) };
        let mut table = Pin::new(Box::<[*const u8]>::from(table));
        let mut bufs = Vec::new();
        for sb in table.iter_mut() {
            let buf = ALLOCATOR.alloc_with_options(
                Layout::from_size_align(page_size, page_size)
                    .map_err(|_| "couldnt allocated scratchpad buffer")?,
            );
            let buf = unsafe { slice::from_raw_parts(buf as *const u8, page_size) };
            let buf = Pin::new(Box::<[u8]>::from(buf));
            *sb = buf.as_ref().as_ptr();
            bufs.push(buf);
        }
        Ok(Self { table, _bufs: bufs })
    }
}

#[repr(C, align(32))]
#[derive(Default, Debug)]
struct EndpointContext {
    data: [u32; 2],
    tr_deque_ptr: Volatile<u64>,
    average_trb_length: u16,
    max_esit_payload_low: u16,
    _reserved: [u32; 3],
}
const _: () = assert!(size_of::<EndpointContext>() == 0x20);
impl EndpointContext {
    fn new() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }
    fn new_control_endpoint(max_packet_size: u16, tr_deque_ptr: u64) -> Result<Self> {
        let mut ep = Self::new();
        ep.set_ep_type(EndpointType::Control)?;
        ep.set_dequeue_cycle_state(true)?;
        ep.set_error_count(3)?;
        ep.set_max_packet_size(max_packet_size);
        ep.set_ring_deque_pointer(tr_deque_ptr)?;
        ep.average_trb_length = 8;
        Ok(ep)
    }
    fn new_bulk_endpoint(
        max_packet_size: u16,
        tr_deque_ptr: u64,
        ep_type: EndpointType,
    ) -> Result<Self> {
        let mut ep = Self::new();
        ep.set_ep_type(ep_type)?;
        ep.set_dequeue_cycle_state(true)?;
        ep.set_error_count(3)?;
        ep.set_max_packet_size(max_packet_size);
        ep.set_ring_deque_pointer(tr_deque_ptr)?;

        // Bulk転送用の設定を追加
        ep.average_trb_length = max_packet_size;

        // 実機対応: Max Burst Sizeの設定
        ep.set_max_burst_size(0)?; // Bulk転送では通常0

        ep.data[0] &= !(0x3 << 0);
        ep.data[0] |= 0 << 0; // Mult = 0

        Ok(ep)
    }
    fn set_max_burst_size(&mut self, burst_size: u8) -> Result<()> {
        if burst_size <= 15 {
            self.data[1] &= !(0xF << 8);
            self.data[1] |= (burst_size as u32) << 8;
            Ok(())
        } else {
            Err("Invalid max burst size")
        }
    }
    fn set_ring_deque_pointer(&mut self, tr_deque_ptr: u64) -> Result<()> {
        self.tr_deque_ptr.write_bits(4, 60, tr_deque_ptr >> 4)
    }
    fn set_max_packet_size(&mut self, max_packet_size: u16) {
        let max_packet_size = max_packet_size as u32;
        self.data[1] &= !(0xffff << 16);
        self.data[1] |= max_packet_size << 16;
    }
    fn set_error_count(&mut self, error_count: u32) -> Result<()> {
        if error_count & !0b11 == 0 {
            self.data[1] &= !(0b11 << 1);
            self.data[1] |= error_count << 1;
            Ok(())
        } else {
            Err("Invalid error count")
        }
    }
    fn set_dequeue_cycle_state(&mut self, dcs: bool) -> Result<()> {
        self.tr_deque_ptr.write_bits(0, 1, dcs.into())
    }
    fn set_ep_type(&mut self, ep_type: EndpointType) -> Result<()> {
        let raw_ep_type = ep_type as u32;
        if raw_ep_type < 8 {
            self.data[1] &= !(0b111 << 3);
            self.data[1] |= raw_ep_type << 3;
            Ok(())
        } else {
            Err("Invalid endpoint type")
        }
    }
}

#[repr(C, align(32))]
#[derive(Default)]
struct DeviceContext {
    slot_ctx: [u32; 8],
    ep_ctx: [EndpointContext; 2 * 15 + 1],
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<DeviceContext>() == 0x400);
impl DeviceContext {
    fn set_port_speed(&mut self, mode: UsbMode) -> Result<()> {
        if mode.psi() < 16u32 {
            self.slot_ctx[0] &= !(0xF << 20);
            self.slot_ctx[0] |= (mode.psi()) << 20;
            Ok(())
        } else {
            Err("Invalid port speed")
        }
    }
    fn set_last_valid_dci(&mut self, dci: usize) -> Result<()> {
        if dci <= 31 {
            self.slot_ctx[0] &= !(0b11111 << 27);
            self.slot_ctx[0] |= (dci as u32) << 27;
            Ok(())
        } else {
            Err("num_ep_ctx out of range")
        }
    }
    fn set_root_hub_port_number(&mut self, port: usize) -> Result<()> {
        if 0 < port && port < 256 {
            self.slot_ctx[1] &= !(0xff << 16);
            self.slot_ctx[1] |= (port as u32) << 16;
            Ok(())
        } else {
            Err("Port out of range")
        }
    }
}

const _: () = assert!(size_of::<DeviceContext>() == 0x400);
#[repr(C, align(4096))]
#[derive(Default)]
struct OutputContext {
    device_ctx: DeviceContext,
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<OutputContext>() <= 4096);

pub struct DeviceContextBaseAddressArray {
    inner: Pin<Box<RawDeviceContextBaseAddressArray>>,
    context: [Option<Pin<Box<OutputContext>>>; 255],
    _scratchpad_buffers: ScratchpadBuffers,
}
impl DeviceContextBaseAddressArray {
    fn new(scratchpad_buffers: ScratchpadBuffers) -> Self {
        let mut inner = RawDeviceContextBaseAddressArray::new();
        inner.scratchpad_table_ptr = scratchpad_buffers.table.as_ref().as_ptr();
        Self {
            inner: Box::pin(inner),
            context: unsafe { MaybeUninit::zeroed().assume_init() },
            _scratchpad_buffers: scratchpad_buffers,
        }
    }
    fn set_output_context(&mut self, slot: u8, output_context: Pin<Box<OutputContext>>) {
        let ctx_idx = slot as usize - 1;
        self.context[ctx_idx] = Some(output_context);
        unsafe {
            self.inner.as_mut().get_unchecked_mut().context[ctx_idx] =
                self.context[ctx_idx]
                    .as_ref()
                    .expect("OutputContext was None")
                    .as_ref()
                    .get_ref() as *const OutputContext as u64;
        }
    }
    fn inner_mut_ptr(&mut self) -> *const RawDeviceContextBaseAddressArray {
        self.inner.as_ref().get_ref() as *const RawDeviceContextBaseAddressArray
    }
}

struct Controller {
    regs: XhcRegisters,
    device_context_base_array: Mutex<DeviceContextBaseAddressArray>,
    primary_event_ring: Mutex<EventRing>,
    command_ring: Mutex<CommandRing>,
}
impl Controller {
    const FEATURE_ENDPOINT_HALT: u16 = 0;

    pub fn new(mut regs: XhcRegisters) -> Result<Self> {
        unsafe {
            regs.op_regs.get_unchecked_mut().reset_xhc();
        }
        let scratchpad_buffers =
            ScratchpadBuffers::alloc(regs.cap_regs.as_ref(), regs.op_regs.as_ref())?;
        let device_context_base_array = DeviceContextBaseAddressArray::new(scratchpad_buffers);
        let device_context_base_array = Mutex::new(device_context_base_array);
        let primary_event_ring = Mutex::new(EventRing::new()?);
        let command_ring = Mutex::new(CommandRing::default());
        let mut xhc = Self {
            regs,
            device_context_base_array,
            primary_event_ring,
            command_ring,
        };
        xhc.init_primary_event_ring()?;
        xhc.init_slots_and_contexts()?;
        xhc.init_command_ring();
        info!("starting xHC...");
        unsafe { xhc.regs.op_regs.get_unchecked_mut() }.start_xhc();
        info!("xHC started runnning!");
        Ok(xhc)
    }
    fn init_primary_event_ring(&mut self) -> Result<()> {
        let eq = &mut self.primary_event_ring;
        unsafe { self.regs.rt_regs.get_unchecked_mut() }.init_irs(0, &mut eq.lock())
    }
    fn init_command_ring(&mut self) {
        unsafe { self.regs.op_regs.get_unchecked_mut() }
            .set_cmd_ring_ctrl(&self.command_ring.lock())
    }
    fn init_slots_and_contexts(&mut self) -> Result<()> {
        let num_slots = self.regs.cap_regs.as_ref().num_of_device_slots();
        unsafe { self.regs.op_regs.get_unchecked_mut() }.set_num_device_slots(num_slots)?;
        unsafe { self.regs.op_regs.get_unchecked_mut() }
            .set_dcbaa_ptr(&mut self.device_context_base_array.lock())
    }
    async fn send_command(&self, cmd: GenericTrbEntry) -> Result<GenericTrbEntry> {
        let cmd_ptr = self.command_ring.lock().push(cmd)?;
        fence(Ordering::SeqCst);
        self.notify_xhc();
        EventFuture::new_for_command(&self.primary_event_ring, cmd_ptr).await
    }
    fn notify_xhc(&self) {
        self.regs.doorbell_regs[0].notify(0, 0)
    }
    pub fn notify_ep(&self, slot: u8, dci: usize) -> Result<()> {
        let db = self
            .regs
            .doorbell_regs
            .get(slot as usize)
            .ok_or("Invalid slot")?;
        let dci = u8::try_from(dci).or(Err("dci out of range"))?;
        db.notify(dci, 0);
        // UnsafeCell を使った内部可変性経由で &mut OperationalRegisters を取得
        let op = unsafe { self.regs.op_regs.as_ref() };
        let sts = op.usbsts.read();
        // xHCI spec: 書き込み “1” でクリアするビットを OR した値を上書き
        const TO_CLEAR: u32 = (1 << 2) // Event Interrupt
                            | (1 << 3) // Port Change Detect
                            | (1 << 4); // Host Controller Event
        op.usbsts.write(sts & !TO_CLEAR); // 念のため残りのフラグは保持したい場合
                                          // または op.usbsts.write(TO_CLEAR); としてビット単体でクリア

        Ok(())
    }
    fn set_output_context_for_slot(&self, slot: u8, output_context: Pin<Box<OutputContext>>) {
        self.device_context_base_array
            .lock()
            .set_output_context(slot, output_context);
    }
    // Configuration設定
    pub async fn request_set_config(
        &self,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
        config_value: u8,
    ) -> Result<()> {
        ctrl_ep_ring.push(
            SetupStageTrb::new(
                0,
                SetupStageTrb::REQ_SET_CONFIGURATION,
                config_value as u16,
                0,
                0,
            )
            .into(),
        )?;

        let trb_ptr = ctrl_ep_ring.push(StatusStageTrb::new_in().into())?;

        self.notify_ep(slot, 1)?;
        EventFuture::new_for_trb(&self.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;
        Ok(())
    }
    async fn request_descriptor<T: Sized>(
        &self,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
        desc_type: UsbDescriptorType,
        desc_index: u8,
        lang_id: u16,
        buf: Pin<&mut [T]>,
    ) -> Result<()> {
        ctrl_ep_ring.push(
            SetupStageTrb::new(
                SetupStageTrb::REQ_TYPE_DIR_DEVICE_TO_HOST,
                SetupStageTrb::REQ_GET_DESCRIPTOR,
                (desc_type as u16) << 8 | (desc_index as u16),
                lang_id,
                (buf.len() * size_of::<T>()) as u16,
            )
            .into(),
        )?;
        let trb_ptr_waiting = ctrl_ep_ring.push(DataStageTrb::new_in(buf).into())?;
        ctrl_ep_ring.push(StatusStageTrb::new_out().into())?;
        self.notify_ep(slot, 1)?;
        EventFuture::new_for_trb(&self.primary_event_ring, trb_ptr_waiting)
            .await?
            .transfer_result_ok()
    }
    /// Clear Feature (ENDPOINT_HALT) リクエストの実装
    pub async fn clear_endpoint_halt(
        &self,
        slot: u8,
        ctrl_ep_ring: &mut CommandRing,
        endpoint_address: u8,
    ) -> Result<()> {
        // Setup Stage: Clear Feature リクエスト
        ctrl_ep_ring.push(
            SetupStageTrb::new(
                SetupStageTrb::REQ_TYPE_DIR_HOST_TO_DEVICE  // bmRequestType: 0x02
                    | SetupStageTrb::REQ_TYPE_TO_ENDPOINT, // Recipient: Endpoint
                SetupStageTrb::REQ_CLEAR_FEATURE, // bRequest: CLEAR_FEATURE (1)
                Self::FEATURE_ENDPOINT_HALT,      // wValue: ENDPOINT_HALT (0)
                endpoint_address as u16,          // wIndex: Endpoint Address
                0,                                // wLength: 0 (no data stage)
            )
            .into(),
        )?;

        // Status Stage (IN direction for no-data control transfer)
        let trb_ptr = ctrl_ep_ring.push(StatusStageTrb::new_in().into())?;

        fence(Ordering::SeqCst);
        // Doorbell通知とイベント待機
        self.notify_ep(slot, 1)?; // Control endpoint DCI = 1
        EventFuture::new_for_trb(&self.primary_event_ring, trb_ptr)
            .await?
            .transfer_result_ok()?;

        Ok(())
    }
}
struct EventRing {
    ring: IoBox<TrbRing>,
    erst: IoBox<EventRingSegmentTableEntry>,
    cycle_state_ours: bool,
    erdp: Option<*mut u64>,
    wait_list: VecDeque<Weak<EventWaitInfo>>,
}
impl EventRing {
    fn new() -> Result<Self> {
        let mut ring = TrbRing::new();
        // セグメント末尾に Link TRB を置いて、リング構造を完成させる
        {
            let link = GenericTrbEntry::trb_link(ring.as_ref());
            let ring_mut = unsafe { ring.get_unchecked_mut() };
            ring_mut.write(TrbRing::NUM_TRB - 1, link)?;
        }
        let erst = EventRingSegmentTableEntry::new(&ring)?;
        Ok(Self {
            ring,
            erst,
            cycle_state_ours: true,
            erdp: None,
            wait_list: Default::default(),
        })
    }
    fn ring_phys_addr(&self) -> u64 {
        self.ring.as_ref() as *const TrbRing as u64
    }
    fn set_erdp(&mut self, erdp: *mut u64) {
        self.erdp = Some(erdp);
    }
    fn erst_phys_addr(&self) -> u64 {
        self.erst.as_ref() as *const EventRingSegmentTableEntry as u64
    }
    fn pop(&mut self) -> Result<Option<GenericTrbEntry>> {
        fence(Ordering::Acquire);
        if !self.has_next_event() {
            return Ok(None);
        }
        let e = self.ring.as_ref().current();
        let eptr = self.ring.as_ref().current_ptr() as u64;
        unsafe { self.ring.get_unchecked_mut() }.advance_index_notoggle(self.cycle_state_ours)?;
        unsafe {
            let erdp = self.erdp.expect("EventRing erdp is not set");
            write_volatile(erdp, eptr | (*erdp & 0b1111));
            fence(Ordering::Release);
        }
        if self.ring.as_ref().current_index() == 0 {
            self.cycle_state_ours = !self.cycle_state_ours;
        }
        Ok(Some(e))
    }
    async fn poll(&mut self) -> Result<()> {
        if let Some(e) = self.pop()? {
            let mut consumed = false;
            for (idx, w) in self.wait_list.iter().enumerate() {
                if let Some(w) = w.upgrade() {
                    let w: &EventWaitInfo = w.as_ref();
                    if w.matches(&e) {
                        w.resolve(&e)?;
                        consumed = true;
                        break; // 最初にマッチしたwaiterのみで停止
                    }
                }
            }
            /*
            if !consumed {
                info!(
                    "Unhandled event: type={}, data={:#x}",
                    e.trb_type(),
                    e.data()
                );
            }*/

            let stale_waiter_indices = self
                .wait_list
                .iter()
                .enumerate()
                .rev()
                .filter_map(|e| -> Option<usize> {
                    if e.1.strong_count() == 0 {
                        Some(e.0)
                    } else {
                        None
                    }
                })
                .collect::<Vec<usize>>();
            for k in stale_waiter_indices {
                self.wait_list.remove(k);
            }
        }
        Ok(())
    }
    fn has_next_event(&self) -> bool {
        fence(Ordering::Acquire);
        self.ring.as_ref().current().cycle_state() == self.cycle_state_ours
    }
    pub fn register_waiter(&mut self, waiter: &Rc<EventWaitInfo>) {
        let wait = Rc::downgrade(waiter);
        self.wait_list.push_back(wait);
    }
}
#[repr(C, align(4096))]
struct EventRingSegmentTableEntry {
    ring_segment_base_address: u64,
    ring_segment_size: u16,
    _rsvdz: [u16; 3],
}
const _: () = assert!(size_of::<EventRingSegmentTableEntry>() == 4096);
impl EventRingSegmentTableEntry {
    fn new(ring: &IoBox<TrbRing>) -> Result<IoBox<Self>> {
        let mut erst: IoBox<Self> = IoBox::new();
        {
            let erst = unsafe { erst.get_unchecked_mut() };
            erst.ring_segment_base_address = ring.as_ref() as *const TrbRing as u64;
            erst.ring_segment_size = ring
                .as_ref()
                .num_trbs()
                .try_into()
                .or(Err("Too large num trbs"))?;
        }
        Ok(erst)
    }
}

#[repr(C, align(4096))]
struct TrbRing {
    trb: [GenericTrbEntry; Self::NUM_TRB],
    current_index: usize,
    _pinned: PhantomPinned,
}

const _: () = assert!(size_of::<TrbRing>() <= 4096);
impl TrbRing {
    const NUM_TRB: usize = 16;
    fn new() -> IoBox<Self> {
        IoBox::new()
    }
    const fn num_trbs(&self) -> usize {
        Self::NUM_TRB
    }
    fn write(&mut self, index: usize, trb: GenericTrbEntry) -> Result<()> {
        if index < self.trb.len() {
            unsafe {
                write_volatile(&mut self.trb[index], trb);
            }
            Ok(())
        } else {
            Err("TrbRing out of range")
        }
    }
    fn phys_addr(&self) -> u64 {
        &self.trb[0] as *const GenericTrbEntry as u64
    }
    fn current_index(&self) -> usize {
        self.current_index
    }
    fn advance_index_notoggle(&mut self, cycle_ours: bool) -> Result<()> {
        if self.current().cycle_state() != cycle_ours {
            return Err("cycle state mismatch");
        }
        self.current_index = (self.current_index + 1) % self.trb.len();
        Ok(())
    }
    fn current(&self) -> GenericTrbEntry {
        self.trb(self.current_index)
    }
    fn trb(&self, index: usize) -> GenericTrbEntry {
        unsafe { read_volatile(&self.trb[index]) }
    }
    fn current_ptr(&self) -> usize {
        &self.trb[self.current_index] as *const GenericTrbEntry as usize
    }
    fn advance_index(&mut self, new_cycle: bool) -> Result<()> {
        if self.current().cycle_state() == new_cycle {
            return Err("cycle state mismatch");
        }
        self.trb[self.current_index].set_cycle_state(new_cycle);
        self.current_index = (self.current_index + 1) % self.trb.len();
        Ok(())
    }
    fn write_current(&mut self, trb: GenericTrbEntry) {
        self.write(self.current_index, trb)
            .expect("Failed to write current TRB");
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
#[non_exhaustive]
#[derive(PartialEq, Eq)]
#[allow(unused)]
enum TrbType {
    Normal = 1,
    SetupStage = 2,
    DataStage = 3,
    StatusStage = 4,
    Link = 6,
    EnableSlotCommand = 9,
    AddressDeviceCommand = 11,
    ConfigureEndpointCommand = 12,
    EvaluateContextCommand = 13,
    NoOpCommand = 23,
    TransferEvent = 32,
    CommandCompletionEvent = 33,
    PortStatusChangeEvent = 34,
    HostControllerEvent = 37,
}

#[derive(Debug, Default, Clone)]
#[repr(C, align(16))]
struct GenericTrbEntry {
    data: Volatile<u64>,
    option: Volatile<u32>,
    control: Volatile<u32>,
}

const _: () = assert!(size_of::<GenericTrbEntry>() == 16);
impl GenericTrbEntry {
    const CTRL_BIT_INTERRUPT_ON_SHORT_PACKET: u32 = 1 << 2;
    const CTRL_BIT_INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
    const CTRL_BIT_IMMEDIATE_DATA: u32 = 1 << 6;

    const CTRL_BIT_DATA_DIR_OUT: u32 = 0 << 16;
    const CTRL_BIT_DATA_DIR_IN: u32 = 1 << 16;
    fn trb_link(ring: &TrbRing) -> Self {
        let mut trb = GenericTrbEntry::default();
        trb.set_trb_type(TrbType::Link);
        trb.data.write(ring.phys_addr());
        trb.set_toggle_cycle(true);
        trb
    }
    fn set_trb_type(&mut self, trb_type: TrbType) {
        self.control.write_bits(10, 6, trb_type as u32).unwrap();
    }
    pub fn set_cycle_state(&mut self, cycle: bool) {
        self.control.write_bits(0, 1, cycle.into()).unwrap();
    }
    fn set_toggle_cycle(&mut self, value: bool) {
        self.control.write_bits(1, 1, value.into()).unwrap();
    }
    fn data(&self) -> u64 {
        self.data.read()
    }
    fn slot_id(&self) -> u8 {
        self.control.read_bits(24, 8).try_into().unwrap()
    }
    fn trb_type(&self) -> u32 {
        self.control.read_bits(10, 6)
    }
    fn cycle_state(&self) -> bool {
        self.control.read_bits(0, 1) != 0
    }
    pub fn cmd_enable_slot() -> Self {
        let mut trb = Self::default();
        trb.set_trb_type(TrbType::EnableSlotCommand);
        trb
    }
    pub fn completion_code(&self) -> u32 {
        self.option.read_bits(24, 8)
    }
    fn cmd_result_ok(&self) -> Result<()> {
        if self.trb_type() != TrbType::CommandCompletionEvent as u32 {
            Err("Not a Command Completion Event")
        } else if self.completion_code() != 1 {
            info!(
                "Command Completion Event with error code: {}",
                self.completion_code()
            );
            Err("Command Completion Event with error code")
        } else {
            Ok(())
        }
    }
    fn transfer_result_ok(&self) -> Result<()> {
        if self.trb_type() != TrbType::TransferEvent as u32 {
            Err("Not a TransferEvent")
        } else if self.completion_code() != 1 && self.completion_code() != 13 {
            info!("Transfer Event with error code: {}", self.completion_code());
            Err("Transfer Event with error code")
        } else {
            Ok(())
        }
    }
    fn set_slot_id(&mut self, slot: u8) {
        self.control.write_bits(24, 8, slot as u32).unwrap();
    }
    fn cmd_address_device(input_context: Pin<&InputContext>, slot: u8) -> Self {
        let mut trb = Self::default();
        trb.set_trb_type(TrbType::AddressDeviceCommand);
        trb.data
            .write(input_context.get_ref() as *const InputContext as u64);
        trb.set_slot_id(slot);
        trb
    }
    fn cmd_configure_endpoint(input_context: Pin<&InputContext>, slot: u8) -> Self {
        let mut trb = Self::default();
        trb.set_trb_type(TrbType::ConfigureEndpointCommand);
        trb.data
            .write(input_context.get_ref() as *const InputContext as u64);
        trb.set_slot_id(slot);
        trb
    }
}

impl From<SetupStageTrb> for GenericTrbEntry {
    fn from(trb: SetupStageTrb) -> GenericTrbEntry {
        unsafe { transmute(trb) }
    }
}
impl From<DataStageTrb> for GenericTrbEntry {
    fn from(trb: DataStageTrb) -> GenericTrbEntry {
        unsafe { transmute(trb) }
    }
}
impl From<StatusStageTrb> for GenericTrbEntry {
    fn from(trb: StatusStageTrb) -> GenericTrbEntry {
        unsafe { transmute(trb) }
    }
}
impl From<NormalTrb> for GenericTrbEntry {
    fn from(trb: NormalTrb) -> GenericTrbEntry {
        unsafe { transmute(trb) }
    }
}

struct CommandRing {
    ring: IoBox<TrbRing>,
    cycle_state_ours: bool,
}
impl CommandRing {
    fn ring_phys_addr(&self) -> u64 {
        self.ring.as_ref() as *const TrbRing as u64
    }
    fn push(&mut self, mut src: GenericTrbEntry) -> Result<u64> {
        let ring = unsafe { self.ring.get_unchecked_mut() };
        if ring.current().cycle_state() != self.cycle_state_ours {
            return Err("Command Ring is Full");
        }

        let dst_ptr = ring.current_ptr();
        // 正しい物理アドレス計算
        let ring_base_phys = ring.phys_addr();
        let ring_base_virt = ring as *const TrbRing as u64;
        let dst_phys_addr = ring_base_phys + (dst_ptr as u64 - ring_base_virt);

        unsafe {
            // 実機対応: TRBエリアを完全にクリア
            core::ptr::write_bytes(dst_ptr as *mut u8, 0, 16);
            fence(Ordering::SeqCst);
        }

        src.set_cycle_state(self.cycle_state_ours);
        ring.write_current(src);

        // 実機対応: 書き込み後に強制同期
        fence(Ordering::SeqCst);

        ring.advance_index(!self.cycle_state_ours)?;
        if ring.current().trb_type() == TrbType::Link as u32 {
            ring.advance_index(!self.cycle_state_ours)?;
            self.cycle_state_ours = !self.cycle_state_ours;
        }

        Ok(dst_phys_addr) // 物理アドレスを返す
    }
}
impl Default for CommandRing {
    fn default() -> Self {
        let mut this = Self {
            ring: TrbRing::new(),
            cycle_state_ours: false,
        };
        let link_trb = GenericTrbEntry::trb_link(this.ring.as_ref());
        unsafe { this.ring.get_unchecked_mut() }
            .write(TrbRing::NUM_TRB - 1, link_trb)
            .expect("Failed to write link TRB");
        this
    }
}
#[derive(Debug, Default)]
struct EventWaitCond {
    trb_type: Option<TrbType>,
    trb_addr: Option<u64>,
    slot: Option<u8>,
}

#[derive(Debug)]
struct EventWaitInfo {
    cond: EventWaitCond,
    trbs: Mutex<VecDeque<GenericTrbEntry>>,
}
impl EventWaitInfo {
    fn matches(&self, trb: &GenericTrbEntry) -> bool {
        // デバッグログを追加
        if let Some(trb_addr) = self.cond.trb_addr {
            if trb.data() != trb_addr {
                info!(
                    "TRB address mismatch: expected {:#x}, got {:#x}",
                    trb_addr,
                    trb.data()
                );
                return false;
            }
        }
        if let Some(trb_type) = self.cond.trb_type {
            if trb.trb_type() != trb_type as u32 {
                info!(
                    "TRB type mismatch: expected {:?}, got {}",
                    trb_type,
                    trb.trb_type()
                );
                return false;
            }
        }
        if let Some(slot) = self.cond.slot {
            if trb.slot_id() != slot {
                info!("Slot ID mismatch: expected {}, got {}", slot, trb.slot_id());
                return false;
            }
        }
        true
    }
    fn resolve(&self, trb: &GenericTrbEntry) -> Result<()> {
        self.trbs.under_locked(&|trbs| -> Result<()> {
            trbs.push_back(trb.clone());
            Ok(())
        })
    }
}

struct PortSc {
    entries: Vec<Rc<PortScEntry>>,
}
impl PortSc {
    fn new(bar0: &BarMem64, cap_regs: &CapabilityRegisters) -> Self {
        let base = unsafe { bar0.addr().add(cap_regs.caplength()).add(0x400) } as *mut u32;
        let num_ports = cap_regs.num_of_ports();
        let mut entries = Vec::new();
        for port in 1..=num_ports {
            let ptr = unsafe { base.add((port - 1) * 4) };
            entries.push(Rc::new(PortScEntry::new(ptr)));
        }
        assert!(entries.len() == num_ports);
        Self { entries }
    }
    fn port_range(&self) -> Range<usize> {
        1..self.entries.len() + 1
    }
    fn get(&self, port: usize) -> Option<Rc<PortScEntry>> {
        self.entries.get(port.wrapping_sub(1)).cloned()
    }
}

#[repr(C)]
struct PortScEntry {
    ptr: Mutex<*mut u32>,
}
impl PortScEntry {
    fn new(ptr: *mut u32) -> Self {
        Self {
            ptr: Mutex::new(ptr),
        }
    }
    fn value(&self) -> u32 {
        let portsc = self.ptr.lock();
        unsafe { read_volatile(*portsc) }
    }
    fn bit(&self, pos: usize) -> bool {
        (self.value() & (1 << pos)) != 0
    }
    fn ccs(&self) -> bool {
        self.bit(0)
    }
    fn assert_bit(&self, pos: usize) {
        const PRESERVE_MASK: u32 = 0b01001111000000011111111111101001;
        let portsc = self.ptr.lock();
        let old = unsafe { read_volatile(*portsc) };
        unsafe { write_volatile(*portsc, (old & PRESERVE_MASK) | (1 << pos)) }
    }
    fn pp(&self) -> bool {
        self.bit(9)
    }
    fn assert_pp(&self) {
        self.assert_bit(9)
    }
    pub fn pr(&self) -> bool {
        self.bit(4)
    }
    pub fn assert_pr(&self) {
        self.assert_bit(4)
    }
    pub async fn reset_port(&self) {
        self.assert_pp();
        while !self.pp() {
            yield_execution().await
        }
        self.assert_pr();
        while self.pr() {
            yield_execution().await;
        }
    }
    pub fn ped(&self) -> bool {
        self.bit(1)
    }
    pub fn is_enabled(&self) -> bool {
        self.pp() && self.ccs() && self.ped() && !self.pr()
    }
    pub fn max_packet_size(&self) -> Result<u16> {
        match self.port_speed() {
            UsbMode::FullSpeed | UsbMode::LowSpeed => Ok(8),
            UsbMode::HighSpeed => Ok(64),
            UsbMode::SuperSpeed => Ok(512),
            _ => Err("unknown Protocol Speed ID"),
        }
    }
    pub fn port_speed(&self) -> UsbMode {
        match extract_bits(self.value(), 10, 4) {
            1 => UsbMode::FullSpeed,
            2 => UsbMode::LowSpeed,
            3 => UsbMode::HighSpeed,
            4 => UsbMode::SuperSpeed,
            v => UsbMode::Unknown(v),
        }
    }
}

pub struct Doorbell {
    ptr: Mutex<*mut u32>,
}
impl Doorbell {
    pub fn new(ptr: *mut u32) -> Self {
        Self {
            ptr: Mutex::new(ptr),
        }
    }
    pub fn notify(&self, target: u8, task: u16) {
        let value = (target as u32) | (task as u32) << 16;
        unsafe {
            write_volatile(*self.ptr.lock(), value);
        }
    }
}

#[derive(Clone)]
struct EventFuture {
    wait_on: Rc<EventWaitInfo>,
    _pinned: PhantomPinned,
}
impl EventFuture {
    fn new(event_ring: &Mutex<EventRing>, cond: EventWaitCond) -> Self {
        let wait_on = EventWaitInfo {
            cond,
            trbs: Default::default(),
        };
        let wait_on = Rc::new(wait_on);
        event_ring.lock().register_waiter(&wait_on);
        Self {
            wait_on,
            _pinned: PhantomPinned,
        }
    }
    fn new_for_trb(event_ring: &Mutex<EventRing>, trb_addr: u64) -> Self {
        let trb_addr = Some(trb_addr);
        Self::new(
            event_ring,
            EventWaitCond {
                trb_type: Some(TrbType::TransferEvent),
                trb_addr,
                ..Default::default()
            },
        )
    }
    // Command用の新しい関数を追加
    fn new_for_command(event_ring: &Mutex<EventRing>, trb_addr: u64) -> Self {
        Self::new(
            event_ring,
            EventWaitCond {
                trb_type: Some(TrbType::CommandCompletionEvent), // Command Completion Eventを指定
                trb_addr: Some(trb_addr),
                slot: None,
            },
        )
    }
}

impl Future for EventFuture {
    type Output = Result<GenericTrbEntry>;

    fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<Result<GenericTrbEntry>> {
        let mut_self = unsafe { self.get_unchecked_mut() };
        if let Some(trb) = mut_self.wait_on.trbs.lock().pop_front() {
            Poll::Ready(Ok(trb))
        } else {
            Poll::Pending
        }
    }
}

#[repr(C, align(32))]
#[derive(Default)]
pub struct InputControlContext {
    drop_context_bitmap: u32,
    add_context_bitmap: u32,
    data: [u32; 6],
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<InputControlContext>() == 0x20);
impl InputControlContext {
    pub fn add_context(&mut self, ici: usize) -> Result<()> {
        if ici < 32 {
            self.add_context_bitmap |= 1 << ici;
            Ok(())
        } else {
            Err("Input context index out of range")
        }
    }
}

#[repr(C, align(4096))]
#[derive(Default)]
pub struct InputContext {
    input_ctrl_ctx: InputControlContext,
    device_ctx: DeviceContext,
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<InputContext>() <= 4096);
impl InputContext {
    fn set_ep_ctx(self: &mut Pin<&mut Self>, dci: usize, ep_ctx: EndpointContext) -> Result<()> {
        unsafe {
            self.as_mut().get_unchecked_mut().device_ctx.ep_ctx[dci - 1] = ep_ctx;
        }
        Ok(())
    }
    fn set_input_ctrl_ctx(
        self: &mut Pin<&mut Self>,
        input_ctrl_ctx: InputControlContext,
    ) -> Result<()> {
        unsafe {
            self.as_mut().get_unchecked_mut().input_ctrl_ctx = input_ctrl_ctx;
        }
        Ok(())
    }
    fn set_port_speed(self: &mut Pin<&mut Self>, psi: UsbMode) -> Result<()> {
        unsafe {
            self.as_mut()
                .get_unchecked_mut()
                .device_ctx
                .set_port_speed(psi)
        }
    }
    fn set_root_hub_port_number(self: &mut Pin<&mut Self>, port: usize) -> Result<()> {
        unsafe { self.as_mut().get_unchecked_mut() }
            .device_ctx
            .set_root_hub_port_number(port)
    }
    fn set_last_valid_dci(self: &mut Pin<&mut Self>, dci: usize) -> Result<()> {
        unsafe { self.as_mut().get_unchecked_mut() }
            .device_ctx
            .set_last_valid_dci(dci)
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
#[derive(PartialEq, Eq)]
pub enum EndpointType {
    IsochOut = 1,
    BulkOut = 2,
    InterruptOut = 3,
    Control = 4,
    IsochIn = 5,
    BulkIn = 6,
    InterruptIn = 7,
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum UsbMode {
    Unknown(u32),
    FullSpeed,
    LowSpeed,
    HighSpeed,
    SuperSpeed,
}

impl UsbMode {
    pub fn psi(&self) -> u32 {
        match *self {
            Self::FullSpeed => 1,
            Self::LowSpeed => 2,
            Self::HighSpeed => 3,
            Self::SuperSpeed => 4,
            Self::Unknown(psi) => psi,
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(u8)]
#[non_exhaustive]
#[allow(unused)]
#[derive(PartialEq, Eq)]
pub enum UsbDescriptorType {
    Device = 1,
    Config = 2,
    String = 3,
    Interface = 4,
    Endpoint = 5,
}

#[derive(Debug, Copy, Clone, Default)]
#[allow(unused)]
#[repr(packed)]
pub struct UsbDeviceDescriptor {
    pub desc_length: u8,
    pub desc_type: u8,
    pub version: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version: u16,
    pub manufacturer_index: u8,
    pub product_index: u8,
    pub serial_idx: u8,
    pub num_of_config: u8,
}

const _: () = assert!(size_of::<UsbDeviceDescriptor>() == 18);
unsafe impl IntoPinnedMutableSlice for UsbDeviceDescriptor {}

#[derive(Copy, Clone)]
#[repr(C, align(16))]
pub struct SetupStageTrb {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    option: u32,
    control: u32,
}
const _: () = assert!(size_of::<SetupStageTrb>() == 16);
impl SetupStageTrb {
    pub const REQ_TYPE_DIR_DEVICE_TO_HOST: u8 = 1 << 7;
    pub const REQ_TYPE_DIR_HOST_TO_DEVICE: u8 = 0 << 7;

    pub const REQ_TYPE_TYPE_CLASS: u8 = 1 << 5;
    pub const REQ_TYPE_TYPE_VENDOR: u8 = 2 << 5;

    pub const REQ_TYPE_TO_DEVICE: u8 = 0;
    pub const REQ_TYPE_TO_INTERFACE: u8 = 1;
    pub const REQ_TYPE_TO_ENDPOINT: u8 = 2;

    pub const REQ_GET_REPORT: u8 = 1;
    pub const REQ_CLEAR_FEATURE: u8 = 1;
    pub const REQ_SET_FEATURE: u8 = 3;
    pub const REQ_GET_DESCRIPTOR: u8 = 6;
    pub const REQ_SET_CONFIGURATION: u8 = 9;
    pub const REQ_SET_INTERFACE: u8 = 11;
    pub const REQ_SET_PROTOCOL: u8 = 0x0b;

    pub fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        const TRT_NO_DATA_STAGE: u32 = 0;
        const TRT_OUT_DATA_STAGE: u32 = 2;
        const TRT_IN_DATA_STAGE: u32 = 3;
        let transfer_type = if length == 0 {
            TRT_NO_DATA_STAGE
        } else if request & Self::REQ_TYPE_DIR_DEVICE_TO_HOST != 0 {
            TRT_IN_DATA_STAGE
        } else {
            TRT_OUT_DATA_STAGE
        };

        Self {
            request_type,
            request,
            value,
            index,
            length,
            option: 8,
            control: transfer_type << 16
                | (TrbType::SetupStage as u32) << 10
                | GenericTrbEntry::CTRL_BIT_IMMEDIATE_DATA,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C, align(16))]
pub struct DataStageTrb {
    buf: u64,
    option: u32,
    control: u32,
}
const _: () = assert!(size_of::<DataStageTrb>() == 16);
impl DataStageTrb {
    pub fn new_in<T: Sized>(buf: Pin<&mut [T]>) -> Self {
        Self {
            buf: buf.as_ptr() as u64,
            option: (buf.len() * size_of::<T>()) as u32,
            control: (TrbType::DataStage as u32) << 10
                | GenericTrbEntry::CTRL_BIT_DATA_DIR_IN
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_SHORT_PACKET,
        }
    }
    pub fn new_out<T: Sized>(buf: Pin<&mut [T]>) -> Self {
        Self {
            buf: buf.as_ptr() as u64,
            option: (buf.len() * size_of::<T>()) as u32,
            control: (TrbType::DataStage as u32) << 10
                | GenericTrbEntry::CTRL_BIT_DATA_DIR_OUT
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_SHORT_PACKET,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C, align(16))]
struct StatusStageTrb {
    reserved: u64,
    option: u32,
    control: u32,
}
const _: () = assert!(size_of::<StatusStageTrb>() == 16);
impl StatusStageTrb {
    fn new_out() -> Self {
        Self {
            reserved: 0,
            option: 0,
            control: (TrbType::StatusStage as u32) << 10,
        }
    }
    fn new_in() -> Self {
        Self {
            reserved: 0,
            option: 0,
            control: (TrbType::StatusStage as u32) << 10
                | GenericTrbEntry::CTRL_BIT_DATA_DIR_IN
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_SHORT_PACKET,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C, align(16))]
pub struct NormalTrb {
    buf: u64,
    option: u32,
    control: u32,
}

impl NormalTrb {
    pub fn new_in<T: Sized>(buf: Pin<&mut [T]>) -> Self {
        Self {
            buf: buf.as_ptr() as u64,
            option: (buf.len() * size_of::<T>()) as u32,
            control: (TrbType::Normal as u32) << 10
                | GenericTrbEntry::CTRL_BIT_DATA_DIR_IN
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_SHORT_PACKET,
        }
    }
    pub fn new_out<T: Sized>(buf: Pin<&mut [T]>) -> Self {
        Self {
            buf: buf.as_ptr() as u64,
            option: (buf.len() * size_of::<T>()) as u32,
            control: (TrbType::Normal as u32) << 10
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_SHORT_PACKET
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION
                | GenericTrbEntry::CTRL_BIT_DATA_DIR_OUT,
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum UsbDescriptor {
    Config(ConfigDescriptor),
    Interface(InterfaceDescriptor),
    Endpoint(EndpointDescriptor),
    Unknown { desc_len: u8, desc_type: u8 },
}

#[derive(Debug, Copy, Clone, Default)]
#[allow(unused)]
#[repr(packed)]
pub struct ConfigDescriptor {
    desc_length: u8,
    desc_type: u8,
    total_length: u16,
    num_of_interfaces: u8,
    config_value: u8,
    config_string_index: u8,
    attributes: u8,
    max_power: u8,
    _pinned: PhantomPinned,
}
const _: () = assert!(size_of::<ConfigDescriptor>() == 9);
impl ConfigDescriptor {
    pub fn total_length(&self) -> usize {
        self.total_length as usize
    }
    pub fn config_value(&self) -> u8 {
        self.config_value
    }
}
unsafe impl IntoPinnedMutableSlice for ConfigDescriptor {}
unsafe impl Sliceable for ConfigDescriptor {}

#[derive(Debug, Copy, Clone, Default)]
#[allow(unused)]
#[repr(packed)]
pub struct InterfaceDescriptor {
    desc_length: u8,
    desc_type: u8,
    interface_number: u8,
    alt_setting: u8,
    num_of_endpoints: u8,
    interface_class: u8,
    interface_subclass: u8,
    interface_protocol: u8,
    interface_index: u8,
}
const _: () = assert!(size_of::<InterfaceDescriptor>() == 9);
unsafe impl IntoPinnedMutableSlice for InterfaceDescriptor {}
unsafe impl Sliceable for InterfaceDescriptor {}
#[derive(Debug, Copy, Clone, Default)]
#[allow(unused)]
#[repr(packed)]
pub struct EndpointDescriptor {
    pub desc_length: u8,
    pub desc_type: u8,
    pub endpoint_address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}
const _: () = assert!(size_of::<EndpointDescriptor>() == 7);
unsafe impl IntoPinnedMutableSlice for EndpointDescriptor {}
unsafe impl Sliceable for EndpointDescriptor {}

pub struct DescriptorIterator<'a> {
    buf: &'a [u8],
    index: usize,
}
impl<'a> DescriptorIterator<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, index: 0 }
    }
}
impl<'a> Iterator for DescriptorIterator<'a> {
    type Item = UsbDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.buf.len() {
            None
        } else {
            let buf = &self.buf[self.index..];
            let desc_len = buf[0];
            let desc_type = buf[1];
            let desc = match desc_type {
                e if e == UsbDescriptorType::Config as u8 => {
                    UsbDescriptor::Config(ConfigDescriptor::copy_from_slice(buf).ok()?)
                }
                e if e == UsbDescriptorType::Interface as u8 => {
                    UsbDescriptor::Interface(InterfaceDescriptor::copy_from_slice(buf).ok()?)
                }
                e if e == UsbDescriptorType::Endpoint as u8 => {
                    UsbDescriptor::Endpoint(EndpointDescriptor::copy_from_slice(buf).ok()?)
                }
                _ => UsbDescriptor::Unknown {
                    desc_len,
                    desc_type,
                },
            };
            self.index += desc_len as usize;
            Some(desc)
        }
    }
}
