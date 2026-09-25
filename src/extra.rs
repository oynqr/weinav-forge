use crate::seed::Seed;
use anyhow::{Context, Result, ensure};

#[path = "extra_tables.rs"]
mod tables;

pub const LENGTH: usize = 6248;

enum Mode {
    BigEndian,
    LowByte,
    Raw,
    Constant,
}
struct Field(usize, usize, Mode, usize, usize);
struct Table {
    key: &'static str,
    count_at: usize,
    src_base: usize,
    src_stride: usize,
    dst_base: usize,
    dst_stride: usize,
    capacity: usize,
    count_slot: usize,
    count_width: usize,
    fields: &'static [Field],
}

fn emit(dst: &mut [u8], src: &[u8], fields: &[Field]) -> Result<()> {
    for Field(offset, width, mode, source, size) in fields {
        let target = dst
            .get_mut(*offset..offset + width)
            .context("EXTRA field exceeds destination")?;
        if matches!(mode, Mode::Constant) {
            target.copy_from_slice(&source.to_le_bytes()[..*width]);
            continue;
        }
        let data = src
            .get(*source..source + size)
            .context("EXTRA field exceeds source")?;
        match mode {
            Mode::Raw => {
                ensure!(width == size, "raw field size mismatch");
                target.copy_from_slice(data);
            }
            Mode::BigEndian => {
                ensure!(width >= size, "EXTRA integer overflow");
                for (d, s) in target.iter_mut().zip(data.iter().rev()) {
                    *d = *s;
                }
            }
            Mode::LowByte => {
                target[0] = *data.last().context("empty low-byte field")?;
            }
            Mode::Constant => unreachable!(),
        }
    }
    Ok(())
}

pub fn build(seed: &Seed) -> Result<(Vec<u8>, Vec<String>)> {
    let mut out = vec![0; LENGTH];
    let mut notes = Vec::new();
    emit(
        &mut out,
        &seed.fields["gpsUtc"],
        &[
            Field(0, 4, Mode::BigEndian, 0, 4),
            Field(4, 4, Mode::BigEndian, 4, 4),
            Field(8, 1, Mode::LowByte, 8, 2),
            Field(9, 1, Mode::LowByte, 10, 2),
            Field(10, 1, Mode::LowByte, 12, 2),
            Field(11, 1, Mode::LowByte, 14, 2),
            Field(12, 1, Mode::LowByte, 16, 2),
            Field(13, 1, Mode::LowByte, 18, 2),
        ],
    )?;
    out[16..24].copy_from_slice(seed.fields["gpsIon"].get(..8).context("short gpsIon")?);
    for (offset, tag) in [(0x18, 1_u32), (0x238, 1), (0x340, 3), (0x448, 2)] {
        out[offset..offset + 4].copy_from_slice(&tag.to_le_bytes());
    }
    out[0x550] = *seed.fields["gpsAlm"].get(1).context("short gpsAlm")?;
    for (offset, key) in [(0x955, "gloAlm"), (0xf79, "bdsAlm")] {
        out[offset..offset + 3]
            .copy_from_slice(seed.fields[key].get(1..4).context("short almanac header")?);
    }
    let gal = &seed.fields["galAlm"];
    let seconds = u32::from_be_bytes(gal.get(1..5).context("short galAlm")?.try_into()?);
    let reference = seconds % 604_800 / 600;
    out[0xc59] = (seconds / 604_800) as u8;
    out[0xc5a] = u8::from(reference > 255);
    if reference > 255 {
        out[0xc5c..0xc5e].copy_from_slice(&(reference as u16).to_le_bytes());
    } else {
        out[0xc5e] = reference as u8;
    }
    out[0xc5f] = *gal.get(5).context("short galAlm fraction")?;
    for table in tables::TABLES {
        let src = &seed.fields[table.key];
        let declared = usize::from(
            *src.get(table.count_at)
                .with_context(|| format!("short {} header", table.key))?,
        );
        let valid = table.src_base + declared * table.src_stride == src.len();
        let count = if valid {
            declared.min(table.capacity)
        } else {
            declared
        };
        out[table.count_slot..table.count_slot + table.count_width]
            .copy_from_slice(&count.to_le_bytes()[..table.count_width]);
        if !valid {
            notes.push(format!(
                "{}: inconsistent count and length; no records emitted",
                table.key
            ));
            continue;
        }
        if count < declared {
            notes.push(format!(
                "{}: clamped {declared} records to {count}",
                table.key
            ));
        }
        for i in 0..count {
            let source = table.src_base + i * table.src_stride;
            let destination = table.dst_base + i * table.dst_stride;
            emit(
                &mut out[destination..destination + table.dst_stride],
                &src[source..source + table.src_stride],
                table.fields,
            )?;
        }
    }
    emit(
        &mut out[0x1858..],
        &seed.fields["seedInfo"],
        &[
            Field(0, 4, Mode::BigEndian, 0, 4),
            Field(4, 4, Mode::BigEndian, 4, 4),
            Field(12, 4, Mode::BigEndian, 8, 1),
        ],
    )?;
    Ok((out, notes))
}
