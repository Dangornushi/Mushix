use core::mem::size_of;

use crate::pin::IntoPinnedMutableSlice;

#[derive(Debug, Copy, Clone)]
#[repr(packed)]
pub struct CommandBlockWrapper {
    pub signature: u32,            // 0x43425355 ("USBC")
    pub tag: u32,                  // コマンド識別子
    pub data_transfer_length: u32, // 転送予定データ長
    pub flags: u8,                 // bit 7: 方向 (1=IN, 0=OUT)
    pub lun: u8,                   // Logical Unit Number (通常0)
    pub cb_length: u8,             // Command Block長 (1-16)
    pub command_block: [u8; 16],   // SCSI Command Block
}

const _: () = assert!(size_of::<CommandBlockWrapper>() == 31);

impl CommandBlockWrapper {
    const SIGNATURE: u32 = 0x43425355; // "USBC"
    const FLAG_DATA_IN: u8 = 0x80;
    const FLAG_DATA_OUT: u8 = 0x00;

    pub fn new_read_10(lba: u32, transfer_length: u16, tag: u32) -> Self {
        let mut cbw = Self {
            signature: Self::SIGNATURE,
            tag,
            data_transfer_length: (transfer_length as u32) * 512, // セクタサイズ512バイト
            flags: Self::FLAG_DATA_IN,
            lun: 0,
            cb_length: 10,
            command_block: [0; 16],
        };

        // SCSI Read(10) コマンド
        cbw.command_block[0] = 0x28; // READ(10) command
        cbw.command_block[1] = 0x00; // LUN & flags
        cbw.command_block[2] = (lba >> 24) as u8; // LBA[31:24]
        cbw.command_block[3] = (lba >> 16) as u8; // LBA[23:16]
        cbw.command_block[4] = (lba >> 8) as u8; // LBA[15:8]
        cbw.command_block[5] = lba as u8; // LBA[7:0]
        cbw.command_block[6] = 0x00; // Group number
        cbw.command_block[7] = (transfer_length >> 8) as u8; // Transfer length[15:8]
        cbw.command_block[8] = transfer_length as u8; // Transfer length[7:0]
        cbw.command_block[9] = 0x00; // Control

        cbw
    }
}

unsafe impl IntoPinnedMutableSlice for CommandBlockWrapper {}
