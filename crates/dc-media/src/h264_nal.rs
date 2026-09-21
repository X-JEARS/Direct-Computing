use dc_common::{DcError, Result};

/// Split an H.264 access unit encoded as Annex-B or four-byte-length AVCC.
pub fn split_h264_nal_units(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    let mut boundaries = Vec::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if bytes[index..].starts_with(&[0, 0, 1]) {
            boundaries.push((index, index + 3));
            index += 3;
        } else if index + 4 <= bytes.len() && bytes[index..].starts_with(&[0, 0, 0, 1]) {
            boundaries.push((index, index + 4));
            index += 4;
        } else {
            index += 1;
        }
    }
    if boundaries.is_empty() {
        return split_avcc(bytes);
    }
    Ok(boundaries
        .iter()
        .enumerate()
        .filter_map(|(position, (_, start))| {
            let end = boundaries
                .get(position + 1)
                .map(|(delimiter, _)| *delimiter)
                .unwrap_or(bytes.len());
            let nal = &bytes[*start..end];
            (!nal.is_empty()).then_some(nal)
        })
        .collect())
}

fn split_avcc(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length_bytes: [u8; 4] = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| DcError::Codec("truncated AVCC NAL length".into()))?
            .try_into()
            .expect("slice length was checked");
        offset += 4;
        let length = u32::from_be_bytes(length_bytes) as usize;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| DcError::Codec("AVCC NAL length overflowed".into()))?;
        let nal = bytes
            .get(offset..end)
            .ok_or_else(|| DcError::Codec("truncated AVCC NAL payload".into()))?;
        output.push(nal);
        offset = end;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_without_including_the_next_start_code() {
        let bytes = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        assert_eq!(
            split_h264_nal_units(&bytes).unwrap(),
            vec![&bytes[4..7], &bytes[10..12]]
        );
    }

    #[test]
    fn splits_four_byte_length_avcc() {
        let bytes = [0, 0, 0, 2, 0x67, 1, 0, 0, 0, 2, 0x68, 2];
        assert_eq!(
            split_h264_nal_units(&bytes).unwrap(),
            vec![&bytes[4..6], &bytes[10..12]]
        );
    }

    #[test]
    fn rejects_truncated_avcc() {
        assert!(split_h264_nal_units(&[0, 0, 0, 4, 1]).is_err());
    }
}
