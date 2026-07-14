#![allow(dead_code)]

use crate::can::CanFrame;

pub const CONNECT_CMD: u8 = 0xFF;
pub const DISCONNECT_CMD: u8 = 0xFE;
pub const GET_STATUS_CMD: u8 = 0xFD;
pub const DOWNLOAD_CMD: u8 = 0xC9;
pub const DOWNLOAD_NEXT_CMD: u8 = 0xD0;
pub const DOWNLOAD_END_CMD: u8 = 0xCF;

pub const SUCCESS_RESPONSE: u8 = 0xFF;
pub const ERROR_RESPONSE: u8 = 0xFE;

pub const CONNECT_SIG_B0: u8 = 0xFF;
pub const CONNECT_SIG_B1: u8 = 0x10;
pub const CONNECT_SIG_B4: u8 = 0x08;
pub const CONNECT_SIG_B5: u8 = 0x00;
pub const CONNECT_SIG_B6: u8 = 0x01;
pub const CONNECT_SIG_B7: u8 = 0x01;

pub const BYTES_PER_PACKET: usize = 7;
pub const MAX_FILE_SIZE: usize = 2_097_152;
pub const ALIGNMENT: usize = 224;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XcpResponseId {
    Exact(u32),
    WildcardBase(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XcpIdPair {
    pub send_id: u32,
    pub response: XcpResponseId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XcpConnectInfo {
    pub active_partition: u8,
    pub target_partition: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XcpPreparedFrame {
    Download([u8; 8]),
    DownloadNext(Vec<u8>),
    End,
}

pub fn resolve_ids(send_id: u32) -> Option<XcpIdPair> {
    if (send_id & 0xFFFF_00FF) != 0x1821_0010 {
        return None;
    }

    let target = (send_id >> 8) & 0xFF;
    let response_id = 0x1821_1000 | target;
    let response = if target == 0xFF {
        XcpResponseId::WildcardBase(0x1821_1000)
    } else {
        XcpResponseId::Exact(response_id)
    };

    Some(XcpIdPair { send_id, response })
}

pub fn response_id_matches(response: XcpResponseId, frame_id: u32) -> bool {
    match response {
        XcpResponseId::Exact(id) => frame_id == id,
        XcpResponseId::WildcardBase(base) => (frame_id & 0xFFFF_FF00) == base,
    }
}

pub fn parse_connect_response(data: &[u8]) -> Option<XcpConnectInfo> {
    if data.len() < 8 {
        return None;
    }
    let valid = data[0] == CONNECT_SIG_B0
        && data[1] == CONNECT_SIG_B1
        && data[4] == CONNECT_SIG_B4
        && data[5] == CONNECT_SIG_B5
        && data[6] == CONNECT_SIG_B6
        && data[7] == CONNECT_SIG_B7;
    valid.then_some(XcpConnectInfo {
        active_partition: data[2],
        target_partition: data[3],
    })
}

pub fn is_ack(data: &[u8]) -> bool {
    data.len() == 1 && data[0] == SUCCESS_RESPONSE
}

pub fn padded_image(mut image: Vec<u8>) -> Result<Vec<u8>, String> {
    if image.is_empty() {
        return Err("XCP file is empty".into());
    }
    if image.len() > MAX_FILE_SIZE {
        return Err("XCP file is larger than 2MB".into());
    }
    let padded_len = image.len().div_ceil(ALIGNMENT) * ALIGNMENT;
    if padded_len > MAX_FILE_SIZE {
        return Err("XCP file is larger than 2MB after 224-byte alignment".into());
    }
    image.resize(padded_len, 0);
    Ok(image)
}

pub fn prepare_download(image: &[u8]) -> Vec<XcpPreparedFrame> {
    let mut frames = Vec::with_capacity(image.len().div_ceil(BYTES_PER_PACKET) + 1);
    let full_packets = image.len() / BYTES_PER_PACKET;
    let reserve = image.len() % BYTES_PER_PACKET;

    for chunk in image.chunks_exact(BYTES_PER_PACKET).take(full_packets) {
        let mut data = [0u8; 8];
        data[0] = DOWNLOAD_CMD;
        data[1..].copy_from_slice(chunk);
        frames.push(XcpPreparedFrame::Download(data));
    }

    if reserve != 0 {
        let offset = full_packets * BYTES_PER_PACKET;
        let mut data = Vec::with_capacity(reserve + 2);
        data.push(DOWNLOAD_NEXT_CMD);
        data.push(reserve as u8);
        data.extend_from_slice(&image[offset..]);
        frames.push(XcpPreparedFrame::DownloadNext(data));
    }

    frames.push(XcpPreparedFrame::End);
    frames
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

pub fn connect_frame(ch: u8, id: u32) -> CanFrame {
    frame_from_data(ch, id, [CONNECT_CMD])
}

pub fn get_status_frame(ch: u8, id: u32) -> CanFrame {
    frame_from_data(ch, id, [GET_STATUS_CMD])
}

pub fn prepared_frame_to_can(ch: u8, id: u32, prepared: &XcpPreparedFrame) -> CanFrame {
    match prepared {
        XcpPreparedFrame::Download(data) => frame_from_data(ch, id, *data),
        XcpPreparedFrame::DownloadNext(data) => frame_from_data(ch, id, data.clone()),
        XcpPreparedFrame::End => frame_from_data(ch, id, [DOWNLOAD_END_CMD]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bcu_xcp_ids_like_source() {
        let ids = resolve_ids(0x1821_FF10).unwrap();
        assert_eq!(ids.response, XcpResponseId::WildcardBase(0x1821_1000));
        assert!(response_id_matches(ids.response, 0x1821_10A5));
    }

    #[test]
    fn parses_new_and_old_connect_responses() {
        let info =
            parse_connect_response(&[0xFF, 0x10, 0x00, 0x01, 0x08, 0x00, 0x01, 0x01]).unwrap();
        assert_eq!(info.target_partition, 1);
        assert!(
            parse_connect_response(&[0xFF, 0x10, 0x00, 0x08, 0x08, 0x00, 0x01, 0x01]).is_some()
        );
    }

    #[test]
    fn prepares_xcp_download_frames() {
        let frames = prepare_download(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            frames[0],
            XcpPreparedFrame::Download([0xC9, 1, 2, 3, 4, 5, 6, 7])
        );
        assert_eq!(frames[1], XcpPreparedFrame::DownloadNext(vec![0xD0, 1, 8]));
        assert_eq!(frames[2], XcpPreparedFrame::End);
    }
}
