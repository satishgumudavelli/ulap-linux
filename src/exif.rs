//! Remove GPS location from a JPEG without re-encoding it (orientation and other EXIF stay).
//! The GPS IFD and the values it points to are zeroed, not just unlinked, so the coordinates are
//! gone from the file. XMP packets that mention GPS are dropped. MakerNotes are not inspected.

fn u16_at(b: &[u8], o: usize, le: bool) -> Option<u16> {
    let s: [u8; 2] = b.get(o..o + 2)?.try_into().ok()?;
    Some(if le { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) })
}

fn u32_at(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let s: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
    Some(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
}

fn type_size(t: u16) -> usize {
    match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

/// Strip the GPS IFD from a TIFF block (the EXIF payload). `Ok(false)` = there was none.
fn strip_tiff(t: &mut [u8]) -> Result<bool, String> {
    let bad = || "malformed EXIF".to_string();
    let le = match t.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err(bad()),
    };
    if u16_at(t, 2, le).ok_or_else(bad)? != 42 {
        return Err(bad());
    }
    let ifd0 = u32_at(t, 4, le).ok_or_else(bad)? as usize;
    let n = u16_at(t, ifd0, le).ok_or_else(bad)? as usize;
    let entry = |k: usize| ifd0 + 2 + 12 * k;
    let Some(k) = (0..n).find(|k| u16_at(t, entry(*k), le) == Some(0x8825)) else { return Ok(false) };
    let gps = u32_at(t, entry(k) + 8, le).ok_or_else(bad)? as usize;

    // zero the GPS IFD: the values it points to, then the directory itself
    let gn = u16_at(t, gps, le).ok_or_else(bad)? as usize;
    for e in 0..gn {
        let eo = gps + 2 + 12 * e;
        let size = type_size(u16_at(t, eo + 2, le).ok_or_else(bad)?) * u32_at(t, eo + 4, le).ok_or_else(bad)? as usize;
        if size > 4 {
            let off = u32_at(t, eo + 8, le).ok_or_else(bad)? as usize;
            t.get_mut(off..off.checked_add(size).ok_or_else(bad)?).ok_or_else(bad)?.fill(0);
        }
    }
    t.get_mut(gps..gps + 2 + 12 * gn + 4).ok_or_else(bad)?.fill(0);

    // remove the pointer entry from IFD0: shift the rest up, move the next-IFD pointer, clear the gap
    let next = u32_at(t, entry(n), le).ok_or_else(bad)?;
    t.get(entry(n) + 4 - 1).ok_or_else(bad)?;
    t.copy_within(entry(k + 1)..entry(n), entry(k));
    let new_next = entry(n - 1);
    let nb = if le { next.to_le_bytes() } else { next.to_be_bytes() };
    t[new_next..new_next + 4].copy_from_slice(&nb);
    t[new_next + 4..new_next + 16].fill(0);
    let cnt = (n as u16 - 1).to_le_bytes();
    let cnt = if le { cnt } else { (n as u16 - 1).to_be_bytes() };
    t[ifd0..ifd0 + 2].copy_from_slice(&cnt);
    Ok(true)
}

/// `Ok(None)`: not a JPEG or no location in it (upload as is). `Ok(Some(bytes))`: stripped copy.
pub fn strip_gps(jpeg: &[u8]) -> Result<Option<Vec<u8>>, String> {
    if jpeg.get(0..2) != Some(&[0xFF, 0xD8]) {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(&jpeg[..2]);
    let (mut pos, mut changed) = (2, false);
    while pos + 4 <= jpeg.len() {
        if jpeg[pos] != 0xFF {
            return Err("malformed JPEG".into());
        }
        let marker = jpeg[pos + 1];
        if marker == 0xFF {
            out.push(0xFF); // fill byte
            pos += 1;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break; // image data / end: copied verbatim below
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            out.extend_from_slice(&jpeg[pos..pos + 2]);
            pos += 2;
            continue;
        }
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        let end = pos + 2 + len;
        let seg = jpeg.get(pos..end).ok_or("malformed JPEG")?;
        let payload = &seg[4..];
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let mut tiff = payload[6..].to_vec();
            if strip_tiff(&mut tiff)? {
                changed = true;
                out.extend_from_slice(&seg[..4 + 6]);
                out.extend_from_slice(&tiff);
            } else {
                out.extend_from_slice(seg);
            }
        } else if marker == 0xE1 && payload.starts_with(b"http://ns.adobe.com/xap/1.0/\0") && payload.windows(3).any(|w| w == b"GPS") {
            changed = true; // XMP packet carrying location: drop it
        } else {
            out.extend_from_slice(seg);
        }
        pos = end;
    }
    out.extend_from_slice(&jpeg[pos.min(jpeg.len())..]);
    Ok(changed.then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JPEG with EXIF (little endian): IFD0 = {Orientation=6, GPSInfo -> GPS IFD}, then EOI.
    fn sample() -> Vec<u8> {
        let mut t: Vec<u8> = vec![];
        t.extend_from_slice(b"II*\0");
        t.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: 2 entries
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]); // orientation = 6
        t.extend_from_slice(&[0x25, 0x88, 4, 0, 1, 0, 0, 0]); // GPSInfo, LONG
        t.extend_from_slice(&38u32.to_le_bytes()); // -> GPS IFD at 38
        t.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        assert_eq!(t.len(), 38);
        // GPS IFD: 1 entry: GPSLatitude (tag 2), RATIONAL x1 at offset 56
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&[2, 0, 5, 0, 1, 0, 0, 0]);
        t.extend_from_slice(&56u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(t.len(), 56);
        t.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]); // "coordinates"
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&t);
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1];
        j.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        j.extend_from_slice(&app1);
        j.extend_from_slice(&[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]); // fake scan data
        j
    }

    #[test]
    fn removes_gps_keeps_orientation() {
        let src = sample();
        let out = strip_gps(&src).unwrap().expect("had GPS");
        assert_eq!(out.len(), src.len());
        assert!(!out.windows(4).any(|w| w == [0xDE, 0xAD, 0xBE, 0xEF]), "coordinates still present");
        assert!(!out.windows(2).any(|w| w == [0x25, 0x88]), "GPS pointer still present");
        let tiff = &out[4 + 2 + 6..];
        assert_eq!(u16_at(tiff, 8, true), Some(1)); // IFD0 now has one entry
        assert_eq!(u16_at(tiff, 10, true), Some(0x0112)); // orientation kept
        assert_eq!(u16_at(tiff, 18, true), Some(6)); // with its value
        assert!(out.ends_with(&[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9])); // image data untouched
        assert_eq!(strip_gps(&out).unwrap(), None); // nothing left to strip
    }

    #[test]
    fn leaves_other_data_alone() {
        assert_eq!(strip_gps(b"not a jpeg").unwrap(), None);
        assert_eq!(strip_gps(&[0xFF, 0xD8, 0xFF, 0xD9]).unwrap(), None);
    }
}

#[cfg(test)]
mod real_file {
    /// `ULAP_TEST_JPEG=in.jpg ULAP_TEST_OUT=out.jpg cargo test real_file -- --ignored`
    #[test]
    #[ignore = "needs a real photo"]
    fn strips_a_real_photo() {
        let (Ok(i), Ok(o)) = (std::env::var("ULAP_TEST_JPEG"), std::env::var("ULAP_TEST_OUT")) else { return };
        let src = std::fs::read(i).unwrap();
        let out = super::strip_gps(&src).unwrap().expect("photo has GPS");
        std::fs::write(o, out).unwrap();
    }
}
