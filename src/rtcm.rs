use crate::bits::{Reader, Writer};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

#[path = "rtcm_fields.rs"]
mod schema;

enum Kind {
    Unsigned,
    Signed,
    SignMagnitude,
}
pub struct Field {
    pub name: &'static str,
    width: usize,
    kind: Kind,
    pub scale: f64,
    pub bias: f64,
}

pub struct Message {
    pub number: u16,
    pub values: BTreeMap<String, f64>,
}

pub fn crc24q(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= 0x186_4cfb;
            }
        }
    }
    crc & 0xff_ffff
}

pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for &byte in bytes {
        crc = crc.rotate_left(8);
        crc ^= u16::from(byte);
        crc ^= (crc & 0xff) >> 4;
        crc ^= crc << 12;
        crc ^= (crc & 0xff) << 5;
    }
    crc
}

pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !payload.is_empty() && payload.len() < 1024,
        "invalid RTCM payload length"
    );
    let mut frame = vec![0xd3, (payload.len() >> 8) as u8, payload.len() as u8];
    frame.extend_from_slice(payload);
    let crc = crc24q(&frame);
    frame.extend_from_slice(&crc.to_be_bytes()[1..]);
    Ok(frame)
}

pub fn payloads(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    let mut offset = 0;
    let mut frames = Vec::new();
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 3)
            .context("truncated RTCM header")?;
        ensure!(
            header[0] == 0xd3 && header[1] & 0xfc == 0,
            "invalid RTCM header at byte {offset}"
        );
        let length = usize::from(header[1]) * 256 + usize::from(header[2]);
        ensure!(length >= 2, "empty RTCM payload");
        let frame = bytes
            .get(offset..offset + length + 6)
            .context("truncated RTCM frame")?;
        let crc = u32::from_be_bytes([0, frame[length + 3], frame[length + 4], frame[length + 5]]);
        ensure!(
            crc24q(&frame[..length + 3]) == crc,
            "RTCM CRC mismatch at byte {offset}"
        );
        frames.push(&frame[3..length + 3]);
        offset += frame.len();
    }
    ensure!(!frames.is_empty(), "empty RTCM stream");
    Ok(frames)
}

impl Message {
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(payload);
        let number = reader.unsigned(12)? as u16;
        let fields =
            schema::fields(number).with_context(|| format!("unsupported RTCM message {number}"))?;
        let mut values = BTreeMap::new();
        for field in fields {
            let raw = match field.kind {
                Kind::Unsigned => reader.unsigned(field.width)? as f64,
                Kind::Signed => reader.signed(field.width)? as f64,
                Kind::SignMagnitude => reader.sign_magnitude(field.width)? as f64,
            };
            values.insert(field.name.into(), (raw - field.bias) * field.scale);
        }
        ensure!(reader.remaining() < 8, "extra RTCM payload bytes");
        if reader.remaining() > 0 {
            ensure!(
                reader.unsigned(reader.remaining())? == 0,
                "nonzero RTCM padding"
            );
        }
        if number == 4056 {
            ensure!(values["tag"] == 6.0, "invalid message 4056 tag");
        }
        Ok(Self { number, values })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let fields = schema::fields(self.number).context("unsupported RTCM message")?;
        let mut writer = Writer::default();
        writer.unsigned(12, u64::from(self.number))?;
        for field in fields {
            let value = *self
                .values
                .get(field.name)
                .with_context(|| format!("missing RTCM field {}", field.name))?;
            let raw = (value / field.scale + field.bias).round_ties_even();
            ensure!(
                raw.is_finite() && raw.abs() <= (1_u64 << 53) as f64,
                "invalid RTCM value"
            );
            match field.kind {
                Kind::Unsigned => {
                    ensure!(raw >= 0.0, "negative unsigned RTCM field");
                    writer.unsigned(field.width, raw as u64)?;
                }
                Kind::Signed => writer.signed(field.width, raw as i64)?,
                Kind::SignMagnitude => writer.sign_magnitude(field.width, raw as i64)?,
            }
        }
        Ok(writer.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_klobuchar_vector() -> Result<()> {
        let names = [
            "alpha0", "alpha1", "alpha2", "alpha3", "beta0", "beta1", "beta2", "beta3", "tag",
        ];
        let values = [
            1.0245e-8, 2.2352e-8, -5.9605e-8, -1.1921e-7, 9.4208e4, 9.8304e4, -1.3107e5, -5.2429e5,
            6.0,
        ];
        let message = Message {
            number: 4056,
            values: names
                .into_iter()
                .zip(values)
                .map(|(k, v)| (k.into(), v))
                .collect(),
        };
        let bytes = frame(&message.encode()?)?;
        assert_eq!(
            bytes,
            [
                0xd3, 0, 0x0b, 0xfd, 0x80, 6, 0x0b, 3, 0xff, 0xfe, 0x2e, 6, 0xfe, 0xf8, 0x49, 0xcf,
                0x19
            ]
        );
        let mut flipped = bytes.clone();
        flipped[5] ^= 1;
        assert!(payloads(&flipped).is_err());
        assert_eq!(crc16(b"123456789"), 0x31c3);
        Ok(())
    }
}
