//! What a phone's own table says of how its data partition is encrypted
//! whole, and the device-mapper target that reads the partition through
//! the storage hardware's decryption.

use anyhow::{Context, Result};
use skyward_block::dm::Target;
use skyward_block::fstab::Entry;
use std::path::{Path, PathBuf};

/// The key is in a directory of this name under the one the phone names.
const KEY_DIR: &str = "key";

/// How the kernel knows the cipher a phone names.
const CIPHERS: [(&str, &str); 2] = [("aes-256-xts", "aes-xts-plain64"), ("adiantum", "xchacha12,aes-adiantum-plain64")];

/// The hardware reads and writes in 4096-byte units, so the partition is
/// mapped in whole multiples of eight 512-byte sectors.
const SECTORS_PER_UNIT: u64 = 8;

/// How a phone says its data partition is encrypted: the cipher, and
/// whether the key is one only the storage hardware can unwrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encryption {
    pub cipher: String,
    pub wrapped: bool,
}

impl Encryption {
    /// Read what an entry of the phone's own table says.
    pub fn of(entry: &Entry) -> Result<Encryption> {
        // Android permits an empty metadata options string: a keydirectory
        // still identifies metadata encryption, with AES-256-XTS as default.
        let said = entry.extra("metadata_encryption").or_else(|| {
            entry.extra("keydirectory").filter(|dir| !dir.is_empty()).map(|_| "")
        }).context("the phone says its data partition is not encrypted whole")?;
        let (name, rest) = said.split_once(':').unwrap_or((said, ""));
        let name = if name.is_empty() { "aes-256-xts" } else { name };
        anyhow::ensure!(rest.is_empty() || rest == "wrappedkey_v0", "unsupported metadata encryption flag {rest}");
        let cipher = CIPHERS
            .iter()
            .find_map(|(theirs, kernels)| (*theirs == name).then_some(*kernels))
            .with_context(|| format!("no cipher here is the {name} the phone names"))?;
        Ok(Encryption { cipher: cipher.to_string(), wrapped: rest == "wrappedkey_v0" })
    }
}

/// Where the phone keeps the key for the filesystem an entry describes.
pub fn key_dir(entry: &Entry) -> Result<PathBuf> {
    // Where the phone's table names no directory, the platform keeps the key
    // in the place Android's own vold looks: the metadata partition's
    // encryption store, which is the one this build of the hardware uses.
    let dir = entry
        .extra("keydirectory")
        .filter(|dir| !dir.is_empty())
        .unwrap_or(DEFAULT_KEY_DIRECTORY);
    Ok(Path::new(dir).join(KEY_DIR))
}

/// Where Android's vold keeps the key a data partition is encrypted with,
/// under the metadata partition, where the table names no directory.
pub const DEFAULT_KEY_DIRECTORY: &str = "/metadata/vold/metadata_encryption";

/// What to try where the entry says nothing at all of how the whole of the
/// partition is encrypted: the cipher every phone of this platform uses,
/// first with the key only the storage hardware can unwrap and then with the
/// key as the store keeps it. Which one it is, the partition says by
/// holding a filesystem behind it.
pub fn unnamed() -> [Encryption; 2] {
    let cipher = CIPHERS[0].1.to_string();
    [Encryption { cipher: cipher.clone(), wrapped: true }, Encryption { cipher, wrapped: false }]
}

/// Build the device-mapper target that reads the partition through the
/// storage hardware's decryption.
pub fn target(device: &str, sectors: u64, key: &[u8], encryption: &Encryption) -> Target {
    Target::DefaultKey {
        sectors: sectors - sectors % SECTORS_PER_UNIT,
        device: device.to_string(),
        cipher: encryption.cipher.clone(),
        key: hex::encode(key),
        wrapped: encryption.wrapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(extras: &str) -> Entry {
        Entry {
            device: "/dev/block/bootdevice/by-name/userdata".into(),
            mount_point: "/data".into(),
            filesystem: "f2fs".into(),
            options: vec!["noatime".into(), "inlinecrypt".into()],
            extras: extras.split(',').map(str::to_string).collect(),
        }
    }

    #[test]
    fn the_phones_own_words_for_the_cipher_become_the_kernels() {
        let e = entry("metadata_encryption=aes-256-xts:wrappedkey_v0,keydirectory=/metadata/vold/metadata_encryption");
        assert_eq!(Encryption::of(&e).unwrap(), Encryption { cipher: "aes-xts-plain64".into(), wrapped: true });
        let plain = entry("metadata_encryption=adiantum");
        assert_eq!(Encryption::of(&plain).unwrap(), Encryption { cipher: "xchacha12,aes-adiantum-plain64".into(), wrapped: false });
    }

    #[test]
    fn keydirectory_without_options_uses_androids_default_metadata_cipher() {
        let e = entry("fileencryption=aes-256-xts:aes-256-cts:v2+inlinecrypt_optimized,keydirectory=/metadata/vold/metadata_encryption,fsverity");
        assert_eq!(Encryption::of(&e).unwrap(), Encryption { cipher: "aes-xts-plain64".into(), wrapped: false });
        assert_eq!(Encryption::of(&entry("metadata_encryption=" )).unwrap(), Encryption { cipher: "aes-xts-plain64".into(), wrapped: false });
        assert!(Encryption::of(&entry("fileencryption=aes-256-xts:aes-256-cts:v2")).is_err());
        assert!(Encryption::of(&entry("keydirectory=")).is_err());
        assert!(Encryption::of(&entry("metadata_encryption=aes-256-xts:unknown")).is_err());
    }

    #[test]
    fn a_cipher_or_a_key_the_phone_does_not_name_is_an_error() {
        assert!(Encryption::of(&entry("quota")).is_err());
        assert!(Encryption::of(&entry("metadata_encryption=rot13")).is_err());
    }

    #[test]
    fn an_entry_that_names_no_key_directory_is_read_at_the_one_android_keeps() {
        // This platform's phones leave the directory out of the table, and
        // vold keeps the key at the same place Android's own does.
        assert_eq!(
            key_dir(&entry("quota")).unwrap(),
            Path::new(DEFAULT_KEY_DIRECTORY).join(KEY_DIR),
            "the metadata encryption store is where the key is"
        );
        assert_eq!(key_dir(&entry("keydirectory=")).unwrap(), Path::new(DEFAULT_KEY_DIRECTORY).join(KEY_DIR));
        assert_eq!(key_dir(&entry("keydirectory=/metadata/vold/other")).unwrap(), Path::new("/metadata/vold/other/key"));
    }

    #[test]
    fn the_key_is_in_a_directory_under_the_one_the_phone_names() {
        let e = entry("keydirectory=/metadata/vold/metadata_encryption");
        assert_eq!(key_dir(&e).unwrap(), Path::new("/metadata/vold/metadata_encryption/key"));
    }

    #[test]
    fn the_partition_is_mapped_in_whole_units_of_the_hardwares_reads() {
        let e = Encryption { cipher: "aes-xts-plain64".into(), wrapped: true };
        let t = target("/dev/block/by-name/userdata", 969_825_751, &[0xAB, 0x01], &e);
        let Target::DefaultKey { sectors, key, cipher, device, wrapped } = t else { panic!("a decrypting target") };
        assert_eq!(sectors, 969_825_744, "a whole number of 4096-byte units");
        assert_eq!(key, "ab01");
        assert_eq!(cipher, "aes-xts-plain64");
        assert_eq!(device, "/dev/block/by-name/userdata");
        assert!(wrapped);
    }
}
