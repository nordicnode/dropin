//! Extract icons from Windows .exe files (PE format).
//!
//! Icons live in the PE resource section: an `RT_GROUP_ICON` directory lists
//! the images and each image payload lives under `RT_ICON`. pelite parses
//! the tree and reassembles a valid .ico; the `image` crate converts it to
//! PNG for the desktop.

use std::io::Cursor;
use std::path::Path;

/// Extract the best icon from a Windows executable and return PNG bytes.
///
/// Returns `None` when the file isn't a PE, has no icons, or anything fails.
/// Never panics on malformed input — every step is fallible.
pub fn extract_icon_png(exe: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(exe).ok()?;
    // Quick PE sanity check before parsing.
    if bytes.len() < 64 || &bytes[0..2] != b"MZ" {
        return None;
    }
    let ico_bytes = reassemble_ico(&bytes)?;
    ico_to_png(&ico_bytes)
}

/// Reassemble the first (largest) icon group of a PE file into .ico bytes.
fn reassemble_ico(pe_bytes: &[u8]) -> Option<Vec<u8>> {
    let file = pelite::PeFile::from_bytes(pe_bytes).ok()?;
    let resources = file.resources().ok()?;
    // Pick the icon group with the largest single image.
    let mut best: Option<(u32, Vec<u8>)> = None;
    for entry in resources.icons() {
        let (_name, group) = entry.ok()?;
        let max_area = group
            .entries()
            .iter()
            .map(|e| {
                let w = if e.bWidth == 0 { 256 } else { e.bWidth as u32 };
                let h = if e.bHeight == 0 {
                    256
                } else {
                    e.bHeight as u32
                };
                w * h
            })
            .max()
            .unwrap_or(0);
        let mut ico = Vec::new();
        group.write(&mut ico).ok()?;
        if ico.is_empty() {
            continue;
        }
        let replace = match &best {
            Some((area, _)) => max_area > *area,
            None => true,
        };
        if replace {
            best = Some((max_area, ico));
        }
    }
    best.map(|(_, ico)| ico)
}

/// Convert .ico bytes to PNG bytes using the `image` crate.
fn ico_to_png(ico: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(ico, image::ImageFormat::Ico).ok()?;
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    if png.is_empty() {
        None
    } else {
        Some(png)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_pe() {
        assert!(extract_icon_png(Path::new("/definitely/not/a/real.exe")).is_none());
        // MZ header but truncated.
        let tmp = std::env::temp_dir().join("dropin-test-fake.exe");
        std::fs::write(&tmp, b"MZgarbage").unwrap();
        assert!(extract_icon_png(&tmp).is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rejects_empty() {
        let tmp = std::env::temp_dir().join("dropin-test-empty.exe");
        std::fs::write(&tmp, b"").unwrap();
        assert!(extract_icon_png(&tmp).is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    /// Build a minimal PE32 with one 1x1 32-bit icon, then verify we get
    /// valid PNG bytes back out.
    #[test]
    fn extracts_icon_from_synthetic_pe() {
        let pe = synthetic_pe_with_icon();
        let tmp = std::env::temp_dir().join("dropin-test-icon.exe");
        std::fs::write(&tmp, &pe).unwrap();
        let png = extract_icon_png(&tmp).expect("should extract icon");
        // PNG magic.
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Minimal PE32: DOS header, PE headers, one .rsrc section holding
    /// RT_GROUP_ICON (type 14) and RT_ICON (type 3) for a 1x1 icon.
    fn synthetic_pe_with_icon() -> Vec<u8> {
        fn u16(v: u16, out: &mut Vec<u8>) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        fn u32(v: u32, out: &mut Vec<u8>) {
            out.extend_from_slice(&v.to_le_bytes());
        }

        // 1x1 32bpp DIB: BITMAPINFOHEADER + 1 pixel + 1 AND-mask row.
        let mut dib = Vec::new();
        u32(40, &mut dib); // biSize
        u32(1, &mut dib); // biWidth
        u32(2, &mut dib); // biHeight (XOR + AND)
        u16(1, &mut dib); // biPlanes
        u16(32, &mut dib); // biBitCount
        u32(0, &mut dib); // biCompression = BI_RGB
        u32(0, &mut dib); // biSizeImage
        u32(0, &mut dib); // biXPelsPerMeter
        u32(0, &mut dib); // biYPelsPerMeter
        u32(0, &mut dib); // biClrUsed
        u32(0, &mut dib); // biClrImportant
        dib.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]); // red pixel BGRA
        dib.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // AND mask row
        assert_eq!(dib.len(), 48);

        // GRPICONDIR + one entry (nId = 1).
        let mut grp = Vec::new();
        u16(0, &mut grp); // idReserved
        u16(1, &mut grp); // idType = icon
        u16(1, &mut grp); // idCount
        grp.push(1); // bWidth
        grp.push(1); // bHeight
        grp.push(0); // bColorCount
        grp.push(0); // bReserved
        u16(1, &mut grp); // wPlanes
        u16(32, &mut grp); // wBitCount
        u32(48, &mut grp); // dwBytesInRes
        u16(1, &mut grp); // nId
        assert_eq!(grp.len(), 20);

        // Resource tree. Layout (offsets relative to .rsrc start):
        //   0: root dir (16) — 2 ID entries: types 3, 14
        //  16: entry type 3 -> subdir at 32
        //  24: entry type 14 -> subdir at 64
        //  32: type-3 dir (16) — 1 ID entry: id 1
        //  48: entry id 1 -> subdir at 80
        //  64: type-14 dir (16) — 1 ID entry: id 1
        //  80: entry id 1 -> subdir at 112
        //  96: (padding to 112)
        // 112: name dir for type 3 (16) — 1 ID entry: lang 1033
        // 128: entry lang 1033 -> data entry at 144
        // 144: name dir for type 14 (16) — 1 ID entry: lang 1033
        // 160: entry lang 1033 -> data entry at 176
        // 176: data entry for RT_ICON (16)
        // 192: data entry for RT_GROUP_ICON (16)
        // 208: RT_ICON payload (48)
        // 256: RT_GROUP_ICON payload (20)
        const RSRC_RVA: u32 = 0x3000;
        let mut rsrc = vec![0u8; 208];

        fn dir(out: &mut [u8], off: usize, named: u16, ids: u16) {
            out[off..off + 16].copy_from_slice(&[0; 16]);
            out[off + 12..off + 14].copy_from_slice(&named.to_le_bytes());
            out[off + 14..off + 16].copy_from_slice(&ids.to_le_bytes());
        }
        fn entry(out: &mut [u8], off: usize, id: u32, target: usize, is_dir: bool) {
            out[off..off + 4].copy_from_slice(&id.to_le_bytes());
            let v = if is_dir {
                (target as u32) | 0x8000_0000
            } else {
                target as u32
            };
            out[off + 4..off + 8].copy_from_slice(&v.to_le_bytes());
        }
        fn data_entry(out: &mut [u8], off: usize, rva: u32, size: u32) {
            out[off..off + 4].copy_from_slice(&rva.to_le_bytes());
            out[off + 4..off + 8].copy_from_slice(&size.to_le_bytes());
            // codepage + reserved stay zero
        }

        dir(&mut rsrc, 0, 0, 2);
        entry(&mut rsrc, 16, 3, 32, true);
        entry(&mut rsrc, 24, 14, 64, true);
        dir(&mut rsrc, 32, 0, 1);
        entry(&mut rsrc, 48, 1, 112, true);
        dir(&mut rsrc, 64, 0, 1);
        entry(&mut rsrc, 80, 1, 144, true);
        dir(&mut rsrc, 112, 0, 1);
        entry(&mut rsrc, 128, 1033, 176, false);
        dir(&mut rsrc, 144, 0, 1);
        entry(&mut rsrc, 160, 1033, 192, false);
        data_entry(&mut rsrc, 176, RSRC_RVA + 208, 48);
        data_entry(&mut rsrc, 192, RSRC_RVA + 256, 20);
        rsrc.extend_from_slice(&dib);
        rsrc.extend_from_slice(&grp);

        // Headers.
        let mut pe = Vec::new();
        // DOS header: "MZ", e_lfanew at 0x3C -> 64.
        pe.extend_from_slice(b"MZ");
        pe.resize(0x3C, 0);
        pe.extend_from_slice(&64u32.to_le_bytes());
        pe.resize(64, 0);
        // PE signature.
        pe.extend_from_slice(b"PE\0\0");
        // COFF header (20 bytes): machine i386, 1 section.
        pe.extend_from_slice(&0x014Cu16.to_le_bytes());
        pe.extend_from_slice(&1u16.to_le_bytes());
        pe.extend_from_slice(&[0u8; 12]); // timestamp, symtab, symcount
        pe.extend_from_slice(&224u16.to_le_bytes()); // size of optional header
        pe.extend_from_slice(&0x010Fu16.to_le_bytes()); // characteristics
                                                        // Optional header PE32 (224 bytes).
        let opt_start = pe.len();
        pe.extend_from_slice(&0x10Bu16.to_le_bytes()); // magic
        pe.resize(opt_start + 224, 0);
        // NumberOfRvaAndSizes at offset 92.
        pe[opt_start + 92..opt_start + 96].copy_from_slice(&16u32.to_le_bytes());
        // DataDirectory[2] (resource table) at offset 96+16 within optional header.
        let dd = opt_start + 96 + 16;
        pe[dd..dd + 4].copy_from_slice(&RSRC_RVA.to_le_bytes());
        pe[dd + 4..dd + 8].copy_from_slice(&(rsrc.len() as u32).to_le_bytes());
        // Section header (40 bytes): ".rsrc".
        let section_start = pe.len();
        let mut name = [0u8; 8];
        name[..5].copy_from_slice(b".rsrc");
        pe.extend_from_slice(&name);
        pe.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
        pe.extend_from_slice(&RSRC_RVA.to_le_bytes()); // VirtualAddress
        pe.extend_from_slice(&(rsrc.len() as u32).to_le_bytes()); // SizeOfRawData
        let raw_ptr = section_start as u32 + 40; // right after this header
        pe.extend_from_slice(&raw_ptr.to_le_bytes()); // PointerToRawData
        pe.extend_from_slice(&[0u8; 12]); // relocs, linenums, counts
        pe.extend_from_slice(&0x40000040u32.to_le_bytes()); // characteristics
        assert_eq!(pe.len() as u32, raw_ptr);
        pe.extend_from_slice(&rsrc);
        pe
    }
}
