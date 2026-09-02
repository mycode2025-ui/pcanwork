//! Reusable byte-grid model helpers for lazily compiled feature windows.

use crate::{FeatureTxByteCell, FeatureTxByteRow, parse_tx_bytes};
use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;

pub(crate) fn build_feature_hex_rows(data: &[u8], requested_len: usize) -> ModelRc<FeatureTxByteRow> {
    let row_count = requested_len.max(1).div_ceil(8);
    let rows = (0..row_count)
        .map(|row| {
            let cells = (0..8)
                .map(|column| {
                    let index = row * 8 + column;
                    FeatureTxByteCell {
                        hex: if index < requested_len {
                            format!("{:02X}", data.get(index).copied().unwrap_or(0)).into()
                        } else {
                            "".into()
                        },
                        valid: index < requested_len,
                        enabled: index < requested_len,
                    }
                })
                .collect::<Vec<_>>();
            FeatureTxByteRow {
                offset: format!("{:02X}", row * 8).into(),
                bytes: ModelRc::from(Rc::new(VecModel::from(cells))),
            }
        })
        .collect::<Vec<_>>();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn edit_feature_hex_byte(
    rows: &ModelRc<FeatureTxByteRow>,
    index: usize,
    value: &str,
) {
    let Some(row) = rows.row_data(index / 8) else { return };
    let Some(mut cell) = row.bytes.row_data(index % 8) else { return };
    if !cell.enabled { return; }
    let normalized = value
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect::<String>()
        .to_ascii_uppercase();
    cell.valid = normalized.len() == 2;
    cell.hex = normalized.into();
    row.bytes.set_row_data(index % 8, cell);
}

pub(crate) fn fill_feature_hex_rows(
    value: &str,
    max_len: usize,
) -> Option<(ModelRc<FeatureTxByteRow>, usize)> {
    let bytes = parse_tx_bytes(value, max_len);
    (!bytes.is_empty()).then(|| {
        let len = bytes.len();
        (build_feature_hex_rows(&bytes, len), len)
    })
}

pub(crate) fn collect_feature_hex_rows(
    rows: &ModelRc<FeatureTxByteRow>,
    length: usize,
) -> Result<Vec<u8>, usize> {
    let mut data = Vec::with_capacity(length);
    for index in 0..length {
        let row = rows.row_data(index / 8).ok_or(index)?;
        let cell = row.bytes.row_data(index % 8).ok_or(index)?;
        if !cell.enabled || !cell.valid { return Err(index); }
        data.push(u8::from_str_radix(&cell.hex, 16).map_err(|_| index)?);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_grid_supports_long_payload_and_validation() {
        let input: Vec<u8> = (0..80).collect();
        let rows = build_feature_hex_rows(&input, input.len());
        assert_eq!(rows.row_count(), 10);
        assert_eq!(collect_feature_hex_rows(&rows, 80).unwrap(), input);
        edit_feature_hex_byte(&rows, 79, "GG");
        assert_eq!(collect_feature_hex_rows(&rows, 80), Err(79));
    }
}
