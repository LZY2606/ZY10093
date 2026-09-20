//! Dependency-free, deterministic USTAR archive writer.
//!
//! Entries are sorted by path, contain normalized numeric owner/group fields,
//! fixed timestamps and no PAX extensions, so identical inputs always produce
//! byte-identical archives.

pub struct TarEntry<'a> {
    pub path: String,
    pub data: &'a [u8],
    pub executable: bool,
}

fn octal(buf: &mut [u8], value: u64) {
    let n = buf.len();
    let mut v = value;
    for i in (0..n.saturating_sub(1)).rev() {
        buf[i] = b'0' + (v & 7) as u8;
        v >>= 3;
    }
    if n > 0 {
        buf[n - 1] = 0;
    }
}

pub fn write_ustar(entries: &[TarEntry]) -> Vec<u8> {
    let mut sorted: Vec<(&String, &&[u8], bool)> =
        entries.iter().map(|e| (&e.path, &e.data, e.executable)).collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = Vec::new();
    for (path, data, executable) in sorted {
        assert!(path.len() <= 100, "tar path too long: {path}");
        let mut hdr = vec![0u8; 512];
        hdr[..path.len()].copy_from_slice(path.as_bytes());
        let mode: u64 = if executable { 0o755 } else { 0o644 };
        octal(&mut hdr[100..108], mode);
        octal(&mut hdr[108..116], 0); // uid
        octal(&mut hdr[116..124], 0); // gid
        octal(&mut hdr[124..136], data.len() as u64); // size
        octal(&mut hdr[136..148], 0); // mtime fixed at epoch
        hdr[148..156].copy_from_slice(b"0000000\0"); // checksum placeholder spaces-ish
        for b in &mut hdr[148..156] {
            *b = b' ';
        }
        hdr[156] = b'0'; // regular file
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        // owner/group names left empty
        let chk: u64 = hdr.iter().map(|b| *b as u64).sum();
        let cs = format!("{chk:06o}\0 ");
        hdr[148..156].copy_from_slice(cs.as_bytes());

        out.extend_from_slice(&hdr);
        out.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            out.extend(std::iter::repeat_n(0u8, 512 - rem));
        }
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

pub fn write_ustar_owned(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let refs: Vec<TarEntry> = entries.iter().map(|(p, d)| TarEntry { path: p.clone(), data: d, executable: false }).collect();
    write_ustar(&refs)
}
