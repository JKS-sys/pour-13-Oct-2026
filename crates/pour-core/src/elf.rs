//! In-place ELF patching for Linux bottles: the program interpreter
//! (`PT_INTERP`) and `DT_RPATH`/`DT_RUNPATH` strings in `.dynstr`.
//! Replacements are always shorter than the originals, so they are written
//! over the old bytes and NUL padded — no section is ever moved.

use crate::relocate::Replacer;

const PT_INTERP: u32 = 3;
const SHT_DYNAMIC: u32 = 6;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;

pub fn is_elf(data: &[u8]) -> bool {
    data.len() >= 20 && &data[..4] == b"\x7fELF"
}

#[derive(Debug, thiserror::Error)]
pub enum ElfError {
    #[error("ELF: {0}")]
    Malformed(&'static str),
    #[error("ELF string too long to fit in place: {0}")]
    TooLong(String),
}

#[derive(Debug, Default, Clone)]
pub struct Patched {
    pub interpreter: Option<(String, String)>,
    pub rpaths: Vec<(String, String)>,
}

struct Rd<'a> {
    d: &'a [u8],
    le: bool,
    is64: bool,
}

impl Rd<'_> {
    fn u16(&self, at: usize) -> Result<u16, ElfError> {
        let b = self
            .d
            .get(at..at + 2)
            .ok_or(ElfError::Malformed("truncated"))?;
        Ok(if self.le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }
    fn u32(&self, at: usize) -> Result<u32, ElfError> {
        let b = self
            .d
            .get(at..at + 4)
            .ok_or(ElfError::Malformed("truncated"))?;
        let a = [b[0], b[1], b[2], b[3]];
        Ok(if self.le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }
    fn u64(&self, at: usize) -> Result<u64, ElfError> {
        let b = self
            .d
            .get(at..at + 8)
            .ok_or(ElfError::Malformed("truncated"))?;
        let a: [u8; 8] = b.try_into().unwrap();
        Ok(if self.le {
            u64::from_le_bytes(a)
        } else {
            u64::from_be_bytes(a)
        })
    }
    /// Address-sized word (u32 on ELF32).
    fn word(&self, at: usize) -> Result<u64, ElfError> {
        if self.is64 {
            self.u64(at)
        } else {
            self.u32(at).map(u64::from)
        }
    }
}

/// Where the interesting strings live: (offset, capacity) pairs.
struct Layout {
    interp: Option<(usize, usize)>,
    rpaths: Vec<(usize, usize)>,
}

fn layout(data: &[u8]) -> Result<Layout, ElfError> {
    let is64 = match data[4] {
        1 => false,
        2 => true,
        _ => return Err(ElfError::Malformed("class")),
    };
    let le = match data[5] {
        1 => true,
        2 => false,
        _ => return Err(ElfError::Malformed("endianness")),
    };
    let r = Rd { d: data, le, is64 };
    let (phoff, shoff, phentsize, phnum, shentsize, shnum) = if is64 {
        (
            r.u64(32)? as usize,
            r.u64(40)? as usize,
            r.u16(54)? as usize,
            r.u16(56)? as usize,
            r.u16(58)? as usize,
            r.u16(60)? as usize,
        )
    } else {
        (
            r.u32(28)? as usize,
            r.u32(32)? as usize,
            r.u16(42)? as usize,
            r.u16(44)? as usize,
            r.u16(46)? as usize,
            r.u16(48)? as usize,
        )
    };
    let mut lay = Layout {
        interp: None,
        rpaths: Vec::new(),
    };
    for i in 0..phnum {
        let at = phoff + i * phentsize;
        if r.u32(at)? == PT_INTERP {
            let (off, size) = if is64 {
                (r.u64(at + 8)? as usize, r.u64(at + 32)? as usize)
            } else {
                (r.u32(at + 4)? as usize, r.u32(at + 16)? as usize)
            };
            if off + size <= data.len() {
                lay.interp = Some((off, size));
            }
        }
    }
    if shoff == 0 || shnum == 0 {
        return Ok(lay);
    }
    for i in 0..shnum {
        let at = shoff + i * shentsize;
        if r.u32(at + 4)? != SHT_DYNAMIC {
            continue;
        }
        let (dyn_off, dyn_size, link, entsize) = if is64 {
            (
                r.u64(at + 24)? as usize,
                r.u64(at + 32)? as usize,
                r.u32(at + 40)? as usize,
                r.u64(at + 56)? as usize,
            )
        } else {
            (
                r.u32(at + 16)? as usize,
                r.u32(at + 20)? as usize,
                r.u32(at + 24)? as usize,
                r.u32(at + 36)? as usize,
            )
        };
        let entsize = if entsize == 0 {
            if is64 {
                16
            } else {
                8
            }
        } else {
            entsize
        };
        // .dynstr section
        let str_at = shoff + link * shentsize;
        let (str_off, str_size) = if is64 {
            (r.u64(str_at + 24)? as usize, r.u64(str_at + 32)? as usize)
        } else {
            (r.u32(str_at + 16)? as usize, r.u32(str_at + 20)? as usize)
        };
        if str_off + str_size > data.len() || dyn_off + dyn_size > data.len() {
            return Err(ElfError::Malformed("dynamic section out of range"));
        }
        let mut e = dyn_off;
        while e + entsize <= dyn_off + dyn_size {
            let tag = r.word(e)? as i64;
            let val = r.word(e + entsize / 2)? as usize;
            if tag == 0 {
                break;
            }
            if (tag == DT_RPATH || tag == DT_RUNPATH) && val < str_size {
                let start = str_off + val;
                let cap = data[start..str_off + str_size]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|p| p + 1)
                    .unwrap_or(str_size - val);
                lay.rpaths.push((start, cap));
            }
            e += entsize;
        }
    }
    Ok(lay)
}

fn cstr(data: &[u8], off: usize, cap: usize) -> String {
    let region = &data[off..off + cap];
    let n = region.iter().position(|&b| b == 0).unwrap_or(region.len());
    String::from_utf8_lossy(&region[..n]).into_owned()
}

fn write_cstr(data: &mut [u8], off: usize, cap: usize, s: &str) -> Result<(), ElfError> {
    if s.len() + 1 > cap {
        return Err(ElfError::TooLong(s.to_string()));
    }
    let region = &mut data[off..off + cap];
    region.fill(0);
    region[..s.len()].copy_from_slice(s.as_bytes());
    Ok(())
}

/// Current interpreter path, if the file has one.
pub fn interpreter(data: &[u8]) -> Option<String> {
    let lay = layout(data).ok()?;
    lay.interp.map(|(o, c)| cstr(data, o, c))
}

/// Rewrite interpreter and rpaths. `new_interp` overrides the interpreter
/// when the old one points into a Homebrew prefix; rpath entries are run
/// through the replacer, entries outside the new prefix are dropped (except
/// `$ORIGIN` ones), `<prefix>/lib` is appended and `lib/gcc/NN` becomes
/// `lib/gcc/current`, mirroring Homebrew's Linux relocation.
pub fn patch(
    data: &mut [u8],
    rep: &Replacer,
    prefix: &str,
    new_interp: Option<&str>,
) -> Result<Patched, ElfError> {
    let lay = layout(data)?;
    let mut out = Patched::default();
    if let Some((off, cap)) = lay.interp {
        let cur = cstr(data, off, cap);
        let wanted = if rep.replace_str(&cur).is_some() {
            new_interp
                .map(str::to_string)
                .or_else(|| rep.replace_str(&cur))
        } else {
            None
        };
        if let Some(w) = wanted {
            if w != cur {
                write_cstr(data, off, cap, &w)?;
                out.interpreter = Some((cur, w));
            }
        }
    }
    let lib = format!("{prefix}/lib");
    for (off, cap) in lay.rpaths {
        let cur = cstr(data, off, cap);
        if rep.replace_str(&cur).is_none() && cur.contains(&lib) {
            continue;
        }
        let mut parts: Vec<String> = cur
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|p| rep.replace_str(p).unwrap_or_else(|| p.to_string()))
            .filter(|p| p.starts_with(prefix) || p.starts_with("$ORIGIN"))
            .map(|p| gcc_current(&p))
            .collect();
        if !parts.iter().any(|p| p == &lib) {
            parts.push(lib.clone());
        }
        let new = parts.join(":");
        if new != cur {
            if new.len() + 1 > cap {
                // Fall back to the minimal rewrite that is guaranteed to fit.
                let minimal = rep.replace_str(&cur).unwrap_or(cur.clone());
                write_cstr(data, off, cap, &minimal)?;
                out.rpaths.push((cur, minimal));
            } else {
                write_cstr(data, off, cap, &new)?;
                out.rpaths.push((cur, new));
            }
        }
    }
    Ok(out)
}

fn gcc_current(p: &str) -> String {
    // .../lib/gcc/14 -> .../lib/gcc/current
    if let Some(i) = p.rfind("/lib/gcc/") {
        let tail = &p[i + 9..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return format!("{}/lib/gcc/current", &p[..i]);
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_elf() {
        assert!(is_elf(
            b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00"
        ));
        assert!(!is_elf(b"not an elf file at all"));
    }

    #[test]
    fn gcc_dir_becomes_current() {
        assert_eq!(
            gcc_current("/opt/pour/opt/gcc/lib/gcc/14"),
            "/opt/pour/opt/gcc/lib/gcc/current"
        );
        assert_eq!(gcc_current("/opt/pour/lib"), "/opt/pour/lib");
    }

    #[test]
    fn patches_a_real_binary_when_available() {
        // Use this test binary itself: it is an ELF on Linux with a PT_INTERP.
        let exe = std::env::current_exe().unwrap();
        let data = std::fs::read(&exe).unwrap();
        if !is_elf(&data) {
            return;
        }
        let interp = interpreter(&data).expect("test binary has an interpreter");
        assert!(interp.contains("ld"));
        // No Homebrew paths in it, so patching changes nothing.
        let mut copy = data.clone();
        let rep = Replacer::new(vec![(
            "/home/linuxbrew/.linuxbrew".into(),
            "/opt/pour".into(),
        )]);
        let p = patch(&mut copy, &rep, "/opt/pour", Some("/opt/pour/lib/ld.so")).unwrap();
        assert!(p.interpreter.is_none());
        assert_eq!(copy, data);
    }
}
