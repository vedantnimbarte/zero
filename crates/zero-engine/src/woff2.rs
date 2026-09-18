//! WOFF2 to a plain sfnt font, so the shaper and rasterizer can read it.
//!
//! Nearly every site that ships a typeface ships it as `.woff2` and nothing
//! else — both sites in the survey corpus that set `@font-face` do — so
//! without this, `font-family` finds the file, cannot read it, and the page
//! falls back to the system font. That is the difference between a site
//! looking like itself and looking approximately like itself.
//!
//! WOFF2 is not merely a compressed font. The container is Brotli-compressed,
//! and inside it `glyf`/`loca` have been *transformed* into a shape that
//! compresses better and has to be rebuilt. `allsorts` does both; what is left
//! here is packing the tables it hands back into the sfnt container that
//! `rustybuzz` and `fontdue` expect, since allsorts models a font as a table
//! provider rather than as a file.

use allsorts::binary::read::ReadScope;
use allsorts::tables::{FontTableProvider, SfntVersion};
use allsorts::woff2::Woff2Font;

/// Rebuild a WOFF2 file as an sfnt. `None` if it is not WOFF2, or is damaged.
pub fn to_sfnt(bytes: &[u8]) -> Option<Vec<u8>> {
    // Cheap rejection first: this is called on every font a page offers, and
    // most of them are already the plain fonts the rest of the engine reads.
    if bytes.len() < 4 || &bytes[..4] != b"wOF2" {
        return None;
    }
    let font = ReadScope::new(bytes).read::<Woff2Font>().ok()?;
    let provider = font.table_provider(0).ok()?;
    let mut tables: Vec<(u32, Vec<u8>)> = provider
        .table_tags()?
        .into_iter()
        .filter_map(|tag| Some((tag, provider.table_data(tag).ok()??.into_owned())))
        .collect();
    // An sfnt's directory is ordered by tag, and a reader is entitled to
    // binary-search it.
    tables.sort_by_key(|(tag, _)| *tag);
    (!tables.is_empty()).then(|| pack(provider.sfnt_version(), &tables))
}

/// Write the sfnt container: an offset table, a directory, then the tables
/// themselves, each padded to a four-byte boundary.
fn pack(sfnt_version: u32, tables: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = tables.len() as u16;
    // `searchRange` and friends describe the binary search over the directory:
    // the largest power of two not exceeding `count`, times the 16-byte entry.
    let entry_selector = (15 - count.leading_zeros()) as u16;
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = count * 16 - search_range;

    let mut out = Vec::new();
    out.extend_from_slice(&sfnt_version.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    // Every table's offset is known before any of them is written: the
    // directory is a fixed size, and each table is padded to four bytes.
    let mut offset = 12 + tables.len() as u32 * 16;
    for (tag, data) in tables {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += padded_len(data.len());
    }
    for (_, data) in tables {
        out.extend_from_slice(data);
        out.resize(out.len() + (padded_len(data.len()) as usize - data.len()), 0);
    }
    out
}

fn padded_len(len: usize) -> u32 {
    (len as u32).div_ceil(4) * 4
}

/// A table's checksum: its bytes read as big-endian `u32`s and summed, with the
/// tail zero-padded. Wrapping is the definition, not an accident.
fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_that_is_not_woff2_is_left_alone() {
        // The plain fonts the embedder supplies go through the same call.
        assert!(to_sfnt(b"").is_none());
        assert!(to_sfnt(b"\x00\x01\x00\x00rest of a truetype file").is_none());
        assert!(to_sfnt(b"OTTO and the rest").is_none());
        assert!(to_sfnt(b"wOFF").is_none(), "WOFF1 is a different container");
        // The right magic but nothing behind it is a damaged file, not a panic.
        assert!(to_sfnt(b"wOF2").is_none());
        assert!(to_sfnt(&[b'w', b'O', b'F', b'2', 0, 0, 0, 0, 9, 9]).is_none());
    }

    #[test]
    fn the_sfnt_header_describes_the_directory_it_writes() {
        let tables = vec![
            (u32::from_be_bytes(*b"cmap"), vec![1, 2, 3]),  // 3 bytes, pads to 4
            (u32::from_be_bytes(*b"glyf"), vec![4; 8]),
            (u32::from_be_bytes(*b"head"), vec![5; 5]),     // 5 bytes, pads to 8
        ];
        let out = pack(0x0001_0000, &tables);

        assert_eq!(&out[..4], &[0, 1, 0, 0]);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 3, "three tables");
        // The largest power of two not over 3 is 2, so 2*16 and selector 1.
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 32);
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 1);
        assert_eq!(u16::from_be_bytes([out[10], out[11]]), 3 * 16 - 32);

        // Each table lands where the directory says, and the last one's
        // padding is present, so the file is a whole number of words.
        let entry = |i: usize| {
            let at = 12 + i * 16;
            let read = |o: usize| u32::from_be_bytes(out[at + o..at + o + 4].try_into().unwrap());
            (read(0), read(4), read(8) as usize, read(12) as usize)
        };
        let (tag, _, offset, len) = entry(0);
        assert_eq!(tag, u32::from_be_bytes(*b"cmap"));
        assert_eq!(offset, 12 + 3 * 16);
        assert_eq!(&out[offset..offset + len], &[1, 2, 3]);
        let (_, _, offset, len) = entry(2);
        assert_eq!(&out[offset..offset + len], &[5; 5]);
        assert_eq!(out.len() % 4, 0, "the file is padded to a word");
        assert_eq!(out.len(), offset + 8, "including after the last table");

        // The checksum is the table's words summed, not a placeholder.
        assert_eq!(entry(1).1, 0x0404_0404u32.wrapping_mul(2));
    }
}
