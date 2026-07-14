#![allow(dead_code)]

use crate::can::CanFrame;

pub const UDS_TIMEOUT_MS: u64 = 1000;
pub const ACO_OTA_CAN_ID: u32 = 0x61A;
pub const SUP_MAX_BLOCK: usize = 10;

pub const TESTER_PRESENT: u8 = 0x3E;
pub const DIAGNOSTIC_SESSION_CONTROL: u8 = 0x10;
pub const SECURITY_ACCESS: u8 = 0x27;
pub const ROUTINE_CONTROL: u8 = 0x31;
pub const REQUEST_DOWNLOAD: u8 = 0x34;
pub const TRANSFER_DATA: u8 = 0x36;
pub const REQUEST_TRANSFER_EXIT: u8 = 0x37;

const ISO_TP_FIRST_FRAME: u8 = 0x10;
const ISO_TP_CONSECUTIVE_FRAME: u8 = 0x20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdsMemoryBlock {
    pub write_addr: u32,
    pub write_len: u32,
    pub crc: u16,
}

pub fn seed_to_key(seed: u32) -> Option<u32> {
    if seed == 0 {
        return None;
    }

    let mask = 0x2020_4143;
    let mut value = seed;
    for _ in 0..35 {
        value = if value & 0x8000_0000 != 0 {
            (value << 1) ^ mask
        } else {
            value << 1
        };
    }
    Some(value)
}

pub fn single_frame(ch: u8, id: u32, payload: &[u8]) -> CanFrame {
    assert!(
        payload.len() <= 7,
        "UDS single frame payload is larger than 7 bytes"
    );
    let mut data = Vec::with_capacity(payload.len() + 1);
    data.push(payload.len() as u8);
    data.extend_from_slice(payload);
    frame_from_data(ch, id, data)
}

pub fn frame_from_data(ch: u8, id: u32, data: impl Into<Vec<u8>>) -> CanFrame {
    CanFrame {
        t: 0.0,
        ch,
        tx: true,
        id,
        ext: true,
        fd: false,
        brs: false,
        remote: false,
        error: false,
        data: data.into(),
    }
}

pub fn tester_present(ch: u8, id: u32) -> CanFrame {
    single_frame(ch, id, &[TESTER_PRESENT, 0x80])
}

pub fn diagnostic_session(ch: u8, id: u32, session: u8) -> CanFrame {
    single_frame(ch, id, &[DIAGNOSTIC_SESSION_CONTROL, session])
}

pub fn security_seed_request(ch: u8, id: u32, level: u8) -> CanFrame {
    single_frame(ch, id, &[SECURITY_ACCESS, level])
}

pub fn security_key_request(ch: u8, id: u32, level: u8, key: u32) -> CanFrame {
    single_frame(
        ch,
        id,
        &[
            SECURITY_ACCESS,
            level,
            (key >> 24) as u8,
            (key >> 16) as u8,
            (key >> 8) as u8,
            key as u8,
        ],
    )
}

pub fn routine_control_start(ch: u8, id: u32, addr: u32, len: u32) -> [CanFrame; 2] {
    [
        frame_from_data(
            ch,
            id,
            [
                ISO_TP_FIRST_FRAME | 0x00,
                0x0D,
                ROUTINE_CONTROL,
                0x01,
                0xFF,
                0x00,
                0x44,
                (addr >> 24) as u8,
            ],
        ),
        frame_from_data(
            ch,
            id,
            [
                ISO_TP_CONSECUTIVE_FRAME | 0x01,
                (addr >> 16) as u8,
                (addr >> 8) as u8,
                addr as u8,
                (len >> 24) as u8,
                (len >> 16) as u8,
                (len >> 8) as u8,
                len as u8,
            ],
        ),
    ]
}

pub fn routine_control_jump(ch: u8, id: u32, routine_id: u16) -> CanFrame {
    single_frame(
        ch,
        id,
        &[
            ROUTINE_CONTROL,
            0x01,
            (routine_id >> 8) as u8,
            routine_id as u8,
        ],
    )
}

pub fn request_download(ch: u8, id: u32, addr: u32, len: u32) -> [CanFrame; 2] {
    [
        frame_from_data(
            ch,
            id,
            [
                ISO_TP_FIRST_FRAME | 0x00,
                0x0B,
                REQUEST_DOWNLOAD,
                0x00,
                0x44,
                (addr >> 24) as u8,
                (addr >> 16) as u8,
                (addr >> 8) as u8,
            ],
        ),
        frame_from_data(
            ch,
            id,
            [
                ISO_TP_CONSECUTIVE_FRAME | 0x01,
                addr as u8,
                (len >> 24) as u8,
                (len >> 16) as u8,
                (len >> 8) as u8,
                len as u8,
            ],
        ),
    ]
}

pub fn transfer_data_block(ch: u8, id: u32, block_counter: u8, data: &[u8]) -> Vec<CanFrame> {
    let block_size = data.len() + 2;
    assert!(
        block_size <= 0x0FFF,
        "UDS transfer block is too large for one ISO-TP message"
    );

    let mut frames = Vec::with_capacity(data.len().div_ceil(7) + 1);
    let first_copy_len = data.len().min(4);
    let mut first = vec![
        ISO_TP_FIRST_FRAME | (((block_size >> 8) as u8) & 0x0F),
        block_size as u8,
        TRANSFER_DATA,
        block_counter.wrapping_add(1),
    ];
    first.extend_from_slice(&data[..first_copy_len]);
    first.resize(8, 0);
    frames.push(frame_from_data(ch, id, first));

    let mut seq = 1u8;
    for chunk in data[first_copy_len..].chunks(7) {
        let mut cf = Vec::with_capacity(chunk.len() + 1);
        cf.push(ISO_TP_CONSECUTIVE_FRAME | (seq & 0x0F));
        cf.extend_from_slice(chunk);
        cf.resize(8, 0);
        frames.push(frame_from_data(ch, id, cf));
        seq = seq.wrapping_add(1);
    }

    frames
}

pub fn request_transfer_exit(ch: u8, id: u32, crc: u16) -> CanFrame {
    single_frame(
        ch,
        id,
        &[REQUEST_TRANSFER_EXIT, (crc >> 8) as u8, crc as u8],
    )
}

pub fn is_positive_response(data: &[u8], service: u8) -> bool {
    data.get(1).copied() == Some(service.wrapping_add(0x40))
}

pub fn is_flow_control_continue(data: &[u8]) -> bool {
    data.first().copied() == Some(0x30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_seed_key_algorithm_from_c_source() {
        assert_eq!(seed_to_key(0), None);
        assert_eq!(seed_to_key(0x1234_5678), Some(0xA715_3389));
    }

    #[test]
    fn builds_download_request_like_uds_source() {
        let frames = request_download(1, ACO_OTA_CAN_ID, 0x1122_3344, 0x5566_7788);
        assert_eq!(
            frames[0].data,
            vec![0x10, 0x0B, 0x34, 0x00, 0x44, 0x11, 0x22, 0x33]
        );
        assert_eq!(frames[1].data, vec![0x21, 0x44, 0x55, 0x66, 0x77, 0x88]);
    }

    #[test]
    fn builds_transfer_data_first_and_consecutive_frames() {
        let frames = transfer_data_block(1, ACO_OTA_CAN_ID, 0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(frames[0].data, vec![0x10, 0x0A, 0x36, 0x01, 1, 2, 3, 4]);
        assert_eq!(frames[1].data, vec![0x21, 5, 6, 7, 8, 0, 0, 0]);
    }
}
