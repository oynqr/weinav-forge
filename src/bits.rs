use anyhow::{Result, ensure};

pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() * 8 - self.position
    }

    pub fn unsigned(&mut self, width: usize) -> Result<u64> {
        ensure!((1..=63).contains(&width), "invalid bit width {width}");
        ensure!(
            self.remaining() >= width,
            "short bit field at bit {}: need {width}, have {}",
            self.position,
            self.remaining()
        );
        let mut value = 0;
        for _ in 0..width {
            value = value << 1
                | u64::from((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    pub fn signed(&mut self, width: usize) -> Result<i64> {
        let value = self.unsigned(width)?;
        Ok(if value & (1 << (width - 1)) != 0 {
            value as i64 - (1_i64 << width)
        } else {
            value as i64
        })
    }

    pub fn sign_magnitude(&mut self, width: usize) -> Result<i64> {
        ensure!((2..=63).contains(&width), "invalid signed magnitude width");
        let negative = self.unsigned(1)? != 0;
        let magnitude = self.unsigned(width - 1)? as i64;
        Ok(if negative { -magnitude } else { magnitude })
    }
}

#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
    position: usize,
}

impl Writer {
    pub fn unsigned(&mut self, width: usize, value: u64) -> Result<()> {
        ensure!((1..=63).contains(&width), "invalid bit width {width}");
        ensure!(
            value < 1_u64 << width,
            "unsigned value {value} exceeds {width} bits"
        );
        for shift in (0..width).rev() {
            if self.position.is_multiple_of(8) {
                self.bytes.push(0);
            }
            self.bytes[self.position / 8] |=
                (((value >> shift) & 1) as u8) << (7 - self.position % 8);
            self.position += 1;
        }
        Ok(())
    }

    pub fn signed(&mut self, width: usize, value: i64) -> Result<()> {
        ensure!((1..=63).contains(&width), "invalid bit width {width}");
        let limit = 1_i64 << (width - 1);
        ensure!(
            (-limit..limit).contains(&value),
            "signed value {value} exceeds {width} bits"
        );
        self.unsigned(width, (value as u64) & ((1_u64 << width) - 1))
    }

    pub fn sign_magnitude(&mut self, width: usize, value: i64) -> Result<()> {
        ensure!((2..=63).contains(&width), "invalid signed magnitude width");
        ensure!(
            value.unsigned_abs() < 1_u64 << (width - 1),
            "signed magnitude overflow"
        );
        self.unsigned(1, u64::from(value < 0))?;
        self.unsigned(width - 1, value.unsigned_abs())
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_fields_cross_byte_boundaries() -> Result<()> {
        let mut writer = Writer::default();
        writer.unsigned(3, 5)?;
        writer.signed(13, -4096)?;
        writer.sign_magnitude(9, -255)?;
        writer.signed(8, -50)?;
        let bytes = writer.finish();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.unsigned(3)?, 5);
        assert_eq!(reader.signed(13)?, -4096);
        assert_eq!(reader.sign_magnitude(9)?, -255);
        assert_eq!(reader.signed(8)?, -50);
        assert!(reader.unsigned(8).is_err());
        Ok(())
    }

    #[test]
    fn invalid_values_are_not_truncated() {
        let mut writer = Writer::default();
        assert!(writer.signed(8, 128).is_err());
        assert!(writer.signed(8, -129).is_err());
        assert!(writer.unsigned(8, 256).is_err());
        assert!(writer.sign_magnitude(8, -128).is_err());
        assert!(writer.unsigned(0, 0).is_err());
    }
}
