//! In-place Mach-O patching: rewrites dylib ids, load paths and rpaths
//! inside load commands without spawning `install_name_tool`.
//!
//! Every replacement Pour makes is shorter than the original (its prefix
//! is shorter than Homebrew's and than the `@@HOMEBREW_*@@` placeholders),
//! so the new string always fits in the command's existing space and the
//! header never has to grow. Fat (universal) files are handled per slice.

use crate::relocate::Replacer;

const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_CIGAM: u32 = 0xcefa_edfe;
const MH_CIGAM_64: u32 = 0xcffa_edfe;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;

const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;

/// Is this a Mach-O or universal binary?
pub fn is_macho(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let m = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    matches!(
        m,
        MH_MAGIC | MH_MAGIC_64 | MH_CIGAM | MH_CIGAM_64 | FAT_MAGIC | FAT_MAGIC_64
    )
}

#[derive(Debug, thiserror::Error)]
pub enum MachoError {
    #[error("Mach-O: {0}")]
    Malformed(&'static str),
    #[error("Mach-O load command path too long to fit in place: {0}")]
    TooLong(String),
}

/// Outcome of patching one file.
#[derive(Debug, Default, Clone)]
pub struct Patched {
    /// Number of load-command strings rewritten.
    pub load_commands: usize,
    /// Old → new pairs, for the receipt.
    pub changes: Vec<(String, String)>,
}

fn rd32(d: &[u8], at: usize, le: bool) -> Result<u32, MachoError> {
    let b = d
        .get(at..at + 4)
        .ok_or(MachoError::Malformed("truncated"))?;
    let a = [b[0], b[1], b[2], b[3]];
    Ok(if le {
        u32::from_le_bytes(a)
    } else {
        u32::from_be_bytes(a)
    })
}

/// Rewrite every load-command path matching the replacer. Mutates `data`.
pub fn patch(data: &mut [u8], rep: &Replacer) -> Result<Patched, MachoError> {
    let mut out = Patched::default();
    if data.len() < 8 {
        return Ok(out);
    }
    let magic = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    match magic {
        FAT_MAGIC | FAT_MAGIC_64 => {
            let nfat = rd32(data, 4, false)? as usize;
            let is64 = magic == FAT_MAGIC_64;
            let arch_size = if is64 { 32 } else { 20 };
            let mut slices = Vec::new();
            for i in 0..nfat {
                let at = 8 + i * arch_size;
                let (off, size) = if is64 {
                    let o = u64::from_be_bytes(
                        data.get(at + 8..at + 16)
                            .ok_or(MachoError::Malformed("fat arch"))?
                            .try_into()
                            .unwrap(),
                    );
                    let s = u64::from_be_bytes(
                        data.get(at + 16..at + 24)
                            .ok_or(MachoError::Malformed("fat arch"))?
                            .try_into()
                            .unwrap(),
                    );
                    (o as usize, s as usize)
                } else {
                    (
                        rd32(data, at + 8, false)? as usize,
                        rd32(data, at + 12, false)? as usize,
                    )
                };
                if off + size > data.len() {
                    return Err(MachoError::Malformed("fat slice out of range"));
                }
                slices.push((off, size));
            }
            for (off, size) in slices {
                let p = patch_thin(&mut data[off..off + size], rep)?;
                out.load_commands += p.load_commands;
                out.changes.extend(p.changes);
            }
            Ok(out)
        }
        _ => patch_thin(data, rep),
    }
}

fn patch_thin(data: &mut [u8], rep: &Replacer) -> Result<Patched, MachoError> {
    let mut out = Patched::default();
    let magic_be = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let (le, is64) = match magic_be {
        MH_CIGAM_64 => (true, true),
        MH_CIGAM => (true, false),
        MH_MAGIC_64 => (false, true),
        MH_MAGIC => (false, false),
        _ => return Ok(out),
    };
    let header = if is64 { 32 } else { 28 };
    let ncmds = rd32(data, 16, le)? as usize;
    let sizeofcmds = rd32(data, 20, le)? as usize;
    let end = header + sizeofcmds;
    if end > data.len() {
        return Err(MachoError::Malformed("load commands out of range"));
    }
    let mut off = header;
    for _ in 0..ncmds {
        if off + 8 > end {
            return Err(MachoError::Malformed("load command header"));
        }
        let cmd = rd32(data, off, le)?;
        let cmdsize = rd32(data, off + 4, le)? as usize;
        if cmdsize < 8 || off + cmdsize > end {
            return Err(MachoError::Malformed("load command size"));
        }
        let has_path = matches!(
            cmd,
            LC_LOAD_DYLIB
                | LC_ID_DYLIB
                | LC_LAZY_LOAD_DYLIB
                | LC_LOAD_WEAK_DYLIB
                | LC_RPATH
                | LC_REEXPORT_DYLIB
                | LC_LOAD_UPWARD_DYLIB
        );
        if has_path {
            let str_off = rd32(data, off + 8, le)? as usize;
            if str_off >= 8 && str_off < cmdsize {
                let region = &mut data[off + str_off..off + cmdsize];
                let cur_len = region.iter().position(|&b| b == 0).unwrap_or(region.len());
                let current = String::from_utf8_lossy(&region[..cur_len]).into_owned();
                if let Some(new) = rep.replace_str(&current) {
                    if new.len() > region.len() - 1 {
                        return Err(MachoError::TooLong(new));
                    }
                    region.fill(0);
                    region[..new.len()].copy_from_slice(new.as_bytes());
                    out.load_commands += 1;
                    out.changes.push((current, new));
                }
            }
        }
        off += cmdsize;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny 64-bit little-endian Mach-O with one LC_LOAD_DYLIB and
    /// one LC_RPATH command.
    fn fake_macho(dylib: &str, rpath: &str) -> Vec<u8> {
        fn cmd(kind: u32, s: &str) -> Vec<u8> {
            let mut body = Vec::new();
            body.extend_from_slice(&12u32.to_le_bytes()); // string offset
            if kind != LC_RPATH {
                // dylib struct is 16 bytes after name offset: timestamp, versions
                body.extend_from_slice(&[0u8; 12]);
            }
            let str_off = 8 + body.len();
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            while (str_off + bytes.len()) % 8 != 0 {
                bytes.push(0);
            }
            let size = (str_off + bytes.len()) as u32;
            let mut c = Vec::new();
            c.extend_from_slice(&kind.to_le_bytes());
            c.extend_from_slice(&size.to_le_bytes());
            // fix the string offset to the real one
            body[..4].copy_from_slice(&(str_off as u32).to_le_bytes());
            c.extend_from_slice(&body);
            c.extend_from_slice(&bytes);
            c
        }
        let c1 = cmd(LC_LOAD_DYLIB, dylib);
        let c2 = cmd(LC_RPATH, rpath);
        let mut h = Vec::new();
        h.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        h.extend_from_slice(&[0u8; 12]); // cputype, subtype, filetype
        h.extend_from_slice(&2u32.to_le_bytes());
        h.extend_from_slice(&((c1.len() + c2.len()) as u32).to_le_bytes());
        h.extend_from_slice(&[0u8; 8]); // flags, reserved
        h.extend(c1);
        h.extend(c2);
        h
    }

    fn rep() -> Replacer {
        Replacer::new(vec![
            ("@@HOMEBREW_CELLAR@@".into(), "/opt/pour/Cellar".into()),
            ("@@HOMEBREW_PREFIX@@".into(), "/opt/pour".into()),
            ("/opt/homebrew".into(), "/opt/pour".into()),
        ])
    }

    #[test]
    fn rewrites_load_commands_in_place() {
        let mut bin = fake_macho(
            "@@HOMEBREW_PREFIX@@/opt/oniguruma/lib/libonig.5.dylib",
            "@@HOMEBREW_CELLAR@@/jq/1.8.2/lib",
        );
        assert!(is_macho(&bin));
        let before = bin.len();
        let p = patch(&mut bin, &rep()).unwrap();
        assert_eq!(p.load_commands, 2);
        assert_eq!(bin.len(), before);
        let text = String::from_utf8_lossy(&bin);
        assert!(text.contains("/opt/pour/opt/oniguruma/lib/libonig.5.dylib\0"));
        assert!(text.contains("/opt/pour/Cellar/jq/1.8.2/lib\0"));
        assert!(!text.contains("@@HOMEBREW"));
        // second pass is a no-op
        assert_eq!(patch(&mut bin, &rep()).unwrap().load_commands, 0);
    }

    #[test]
    fn leaves_system_paths_alone() {
        let mut bin = fake_macho("/usr/lib/libSystem.B.dylib", "@loader_path/../lib");
        let p = patch(&mut bin, &rep()).unwrap();
        assert_eq!(p.load_commands, 0);
    }

    #[test]
    fn refuses_to_grow() {
        let mut bin = fake_macho("/opt/homebrew/lib/x.dylib", "/opt/homebrew/lib");
        let longer = Replacer::new(vec![(
            "/opt/homebrew".into(),
            "/a/very/much/longer/prefix/than/before".into(),
        )]);
        assert!(matches!(
            patch(&mut bin, &longer),
            Err(MachoError::TooLong(_))
        ));
    }
}
