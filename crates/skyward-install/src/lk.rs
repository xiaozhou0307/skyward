//! Putting the bootloader's own partition back as it was after an install.
//!
//! A package may carry `lk.img` and write it over the bootloader's partition.
//! On a phone whose bootloader must stay the one it was flashed with — an
//! unlocked or otherwise patched one — that write must not be what the phone
//! is left with. Nothing sits between a package's own script and the block
//! devices to stop the write as it is made, so it is undone afterwards: what
//! the partition held before the install is kept whole, in the phone's memory
//! and in a copy under [`BACKUPS`], and after the install the partition is
//! read again — put back byte for byte where a write reached it, and said to
//! be as it was where none did.
//!
//! The copy under [`BACKUPS`] is the phone owner's: if an install is cut off
//! in the middle of putting the partition back, the same bytes are there to
//! write with `fastboot flash`.

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where a copy of what the partition held before an install is kept: for the
/// phone's owner to have, and for putting back what a write got through to.
pub const BACKUPS: &str = "/sdcard/Skyward/lk-backup";

/// Whether `name` is a bootloader partition's: `lk`, or that with a slot's
/// suffix, as the slots of an A/B phone have.
pub fn is_lk(name: &str) -> bool {
    name == "lk" || name.strip_prefix("lk_").is_some_and(|slot| slot == "a" || slot == "b")
}

/// One partition, as it was before the install.
#[derive(Debug)]
struct One {
    name: String,
    real: PathBuf,
    before: Vec<u8>,
}

/// The bootloader partitions of a phone, as they were before an install, to be
/// put back after it.
#[derive(Debug)]
pub struct Guard {
    ones: Vec<One>,
}

impl Guard {
    /// Keep what every bootloader partition holds, before an install: in the
    /// phone's memory to write back, and in a copy under [`BACKUPS`] for the
    /// owner.
    #[cfg(target_os = "android")]
    pub fn protect() -> Result<Guard> {
        let names = names()?;
        ensure!(!names.is_empty(), "no bootloader partition is among the links in {}", skyward_block::BY_NAME);
        let backup = backup_folder()?;
        let mut ones = Vec::new();
        for name in names {
            let real = Path::new(skyward_block::BY_NAME).join(&name);
            let before = read(&real, &name)?;
            let copy = backup.join(format!("{name}.img"));
            std::fs::write(&copy, &before).with_context(|| format!("keep a copy of {name} at {}", copy.display()))?;
            ones.push(One { name, real, before });
        }
        Ok(Guard { ones })
    }

    /// Put each partition back as it was where the install wrote over it, and
    /// say what became of each.
    pub fn release(self) -> Result<Vec<String>> {
        let mut told = Vec::new();
        for One { name, real, before } in self.ones {
            let now = match read(&real, &name) {
                Ok(now) => now,
                Err(e) => {
                    told.push(format!("{name}: could not be read after the install: {e:#}; the copy taken before it is in {BACKUPS}"));
                    continue;
                }
            };
            if !needs_restoring(&before, &now) {
                told.push(format!("{name}: not written by the install (digest {}), and left as it was", digest(&now)));
                continue;
            }
            ensure!(
                before.len() == now.len(),
                "{name} is {} bytes now and was {} before; the copy taken before the install is in {BACKUPS}",
                now.len(),
                before.len()
            );
            told.push(format!("{name}: written over by the install, putting back {} bytes (digest {})", before.len(), digest(&before)));
            write(&real, &before).with_context(|| format!("put {name} back as it was"))?;
            let after = read(&real, &name)?;
            ensure!(
                same(&before, &after),
                "{name} is still not what it was; the copy taken before the install is in {BACKUPS}"
            );
            told.push(format!("{name}: back as it was (digest {})", digest(&after)));
        }
        Ok(told)
    }

    /// What is under the guard, for the log before an install runs.
    pub fn names(&self) -> Vec<String> {
        self.ones.iter().map(|o| o.name.clone()).collect()
    }
}

/// Take a copy of the bootloader partitions where the screen asked it of the
/// phone, and give nothing where it did not, or where there is no phone to do
/// it on.
pub fn protect(wanted: bool) -> Result<Option<Guard>> {
    #[cfg(target_os = "android")]
    {
        if wanted {
            return Guard::protect().map(Some);
        }
        Ok(None)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = wanted;
        Ok(None)
    }
}

/// Put the partitions back where the install wrote over them, saying in the
/// kernel's log what became of each.
pub fn release(guard: Option<Guard>) -> Result<()> {
    let Some(guard) = guard else {
        return Ok(());
    };
    for line in guard.release()? {
        let _ = skyward_sys::kmsg(&format!("install: {line}"));
    }
    Ok(())
}

/// The bootloader partitions this phone has, in the order their names come.
pub fn names() -> Result<Vec<String>> {
    let dir = std::fs::read_dir(skyward_block::BY_NAME).with_context(|| format!("read {}", skyward_block::BY_NAME))?;
    let mut names: Vec<String> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_lk(name))
        .collect();
    names.sort();
    Ok(names)
}

/// The folder the copies taken before an install go to, made where it is not.
pub fn backup_folder() -> Result<PathBuf> {
    let dir = PathBuf::from(BACKUPS);
    std::fs::create_dir_all(&dir).with_context(|| format!("make {}", dir.display()))?;
    Ok(dir)
}

/// Whether what the partition holds now is not what it held before.
pub fn needs_restoring(before: &[u8], now: &[u8]) -> bool {
    !same(before, now)
}

/// Whether two copies of a partition are the same, byte for byte.
pub fn same(one: &[u8], other: &[u8]) -> bool {
    one == other
}

/// The digest of a partition, for the log.
pub fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// All of what `path` holds.
fn read(path: &Path, name: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let size = skyward_block::size_bytes(path).with_context(|| format!("ask how long {name} is"))?;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {name}"))?;
    let mut bytes = Vec::with_capacity(size as usize);
    file.by_ref().take(size).read_to_end(&mut bytes).with_context(|| format!("read {name}"))?;
    ensure!(bytes.len() as u64 == size, "{name} gave {} of its {size} bytes", bytes.len());
    Ok(bytes)
}

/// Write `bytes` over what `path` holds, as the phone's own recovery writes a
/// partition image.
#[cfg(target_os = "android")]
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).open(path).with_context(|| format!("open {}", path.display()))?;
    file.write_all(bytes).with_context(|| format!("write {}", path.display()))?;
    file.sync_all().with_context(|| format!("flush {}", path.display()))?;
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn write(_path: &Path, _bytes: &[u8]) -> Result<()> {
    anyhow::bail!("a partition is written only on the phone")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bootloader_partition_is_named_lk_and_nothing_else_is() {
        assert!(is_lk("lk"));
        assert!(is_lk("lk_a"));
        assert!(is_lk("lk_b"));
        assert!(!is_lk("lkm"), "a modem's own partition is not the bootloader's");
        assert!(!is_lk("lk_c"));
        assert!(!is_lk("lkb"));
        assert!(!is_lk("boot_a"));
    }

    #[test]
    fn a_partition_left_as_it_was_is_not_written_again_and_one_changed_is() {
        let before = vec![1u8, 2, 3, 4];
        assert!(!needs_restoring(&before, &before.clone()), "an install that left it alone needs nothing put back");
        assert!(needs_restoring(&before, &[1u8, 2, 3, 9]), "a write that reached it is put back");
        assert!(needs_restoring(&before, &[0u8; 4]), "a zeroed partition is put back");
        assert_eq!(digest(&before), digest(&[1u8, 2, 3, 4]));
        assert_ne!(digest(&before), digest(&[1u8, 2, 3, 9]));
    }
}
