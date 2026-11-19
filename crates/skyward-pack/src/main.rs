use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use skyward_config::ramdisk as paths;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

mod avb;
mod described;

use skyward_bootimg::{BootImage, Ramdisk, VendorBoot};

/// Builds Skyward recovery images from a device's own stock images.
#[derive(Parser)]
#[command(name = "skyward-pack")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print an image's header and the files in its ramdisk.
    List { image: PathBuf },
    /// Write one ramdisk file to stdout.
    Cat { image: PathBuf, path: String },
    /// Unpack and pack an image's ramdisk unchanged, to check that a rebuilt
    /// archive keeps every entry.
    Repack { image: PathBuf, out: PathBuf },
    /// Join a kernel and another image's ramdisk into one boot image.
    Boot {
        /// Image holding the kernel (the device's boot partition).
        #[arg(long)]
        kernel: PathBuf,
        /// Image holding the ramdisk.
        #[arg(long)]
        ramdisk: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a Skyward recovery image on top of the device's stock recovery:
    /// its init, adbd, SELinux policy and libraries stay, the recovery
    /// service becomes Skyward. The image fills the whole partition and ends
    /// in an unsigned AVB footer of its own.
    Recovery {
        /// The device's stock recovery image: the whole partition, as the
        /// phone or a full update package holds it, ending in its AVB footer.
        #[arg(long)]
        stock: PathBuf,
        /// Skyward built for the device (aarch64-linux-android).
        #[arg(long)]
        binary: PathBuf,
        /// The first program of a system installed beside the installed
        /// one, skyward-system-init built for the device and linked
        /// statically.
        #[arg(long)]
        system_init: PathBuf,
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// The UI typeface.
        #[arg(long)]
        font: PathBuf,
        /// The terminal's monospaced typeface.
        #[arg(long)]
        mono: PathBuf,
        /// The monospaced typeface's licence, which goes with it.
        #[arg(long)]
        mono_license: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Replace the RECOVERY ramdisk fragment in a stock vendor_boot v4 image.
    VendorRecovery {
        #[arg(long)]
        stock: PathBuf,
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        system_init: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        font: PathBuf,
        #[arg(long)]
        mono: PathBuf,
        #[arg(long)]
        mono_license: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the stock build a device's recovery is made on, as lines of
    /// name=value: its version, where its full update package is, and the
    /// SHA-256 of the recovery partition in that package. Given somewhere
    /// to write it, also take that partition out of the package on its
    /// server, reading only what the partition takes, and write it there
    /// once its SHA-256 is that one.
    Stock {
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the stock recovery image.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Take what a device's own partitions say of the programs they carry
    /// out of its stock build's full update package on its server: the
    /// init scripts, the service descriptions, the accounts and the build
    /// properties of each partition that carries such programs, written
    /// under `out` as the partition and then the path in it. Only the
    /// parts of the package that hold these files are downloaded, each
    /// checked against the package's own sum of it.
    Described {
        /// The device configuration, devices/<codename>.toml.
        #[arg(long)]
        config: PathBuf,
        /// A partition to take them out of; vendor, odm and system_ext
        /// when none is named.
        #[arg(long = "partition")]
        partitions: Vec<String>,
        /// The folder to write the files under, which is not there yet.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::List { image } => list(&image),
        Cmd::Cat { image, path } => {
            let rd = Ramdisk::decode(&BootImage::read(&image)?.ramdisk)?;
            let e = rd
                .find(&path)
                .with_context(|| format!("{path} is not in the ramdisk"))?;
            std::io::stdout().write_all(&e.data)?;
            Ok(())
        }
        Cmd::Repack { image, out } => {
            let mut img = BootImage::read(&image)?;
            img.ramdisk = Ramdisk::decode(&img.ramdisk)?.encode()?;
            write(&out, &img, &img.to_bytes()?)
        }
        Cmd::Boot {
            kernel,
            ramdisk,
            out,
        } => {
            let mut img = BootImage::read(&ramdisk)?;
            img.kernel = BootImage::read(&kernel)?.kernel;
            write(&out, &img, &img.to_bytes()?)
        }
        Cmd::Recovery {
            stock,
            binary,
            system_init,
            config,
            font,
            mono,
            mono_license,
            out,
        } => {
            let size = partition_size(&stock)?;
            let mut img = BootImage::read(&stock)?;
            let mut rd = Ramdisk::decode(&img.ramdisk)?;
            // A stock recovery ramdisk has no third-party tree; refuse to build
            // on top of another custom recovery.
            if let Some(e) = rd
                .entries
                .iter()
                .find(|e| e.name == "FFiles" || e.name.starts_with("twres"))
            {
                anyhow::bail!(
                    "{} holds {}, which is not a stock recovery",
                    stock.display(),
                    e.name
                );
            }
            let read =
                |p: &PathBuf| std::fs::read(p).with_context(|| format!("read {}", p.display()));
            skyward_config::DeviceConfig::load(&config)?;
            rd.replace(paths::RECOVERY, read(&binary)?)?;
            // Install scripts look for the commands a stock recovery lacks
            // among its programs, and its unzip there too, a link to ziptool
            // until it points here; the recovery binary is each of them.
            let (_, recovery) = paths::RECOVERY
                .rsplit_once('/')
                .expect("the recovery binary is in a folder");
            for command in paths::COMMANDS {
                rd.link(command, recovery)?;
            }
            rd.add(paths::SYSTEM_INIT, 0o755, read(&system_init)?)?;
            rd.add(paths::CONFIG, 0o644, read(&config)?)?;
            rd.add(paths::FONT, 0o644, read(&font)?)?;
            rd.add(paths::MONO, 0o644, read(&mono)?)?;
            rd.add(paths::MONO_LICENSE, 0o644, read(&mono_license)?)?;
            // A stock ramdisk may run logd without any scheduling profiles,
            // and logd stops as soon as it cannot find the one it asks for.
            if rd.find(paths::TASK_PROFILES).is_none() {
                rd.add(paths::TASK_PROFILES, 0o644, TASK_PROFILES.to_vec())?;
            }
            rd.add(paths::MTP_RC, 0o644, MTP_RC.to_vec())?;
            // On an unlocked device adbd keeps root when ro.secure is 0 and
            // lets any computer in without authorization when ro.adb.secure is
            // 0; init lets a later line of a property file override an earlier
            // one. update_engine refuses a package built before the recovery
            // itself unless both of the last two are set; the recovery checks
            // packages against the installed system instead.
            let stock_props = &rd
                .find(paths::PROP_DEFAULT)
                .with_context(|| format!("{} is not in the ramdisk", paths::PROP_DEFAULT))?
                .data;
            let mut props = without(stock_props, &BUILD_VERSION);
            props.extend_from_slice(
                b"\nro.secure=0\nro.adb.secure=0\nro.debuggable=1\nro.ota.allow_downgrade=true\n",
            );
            rd.replace(paths::PROP_DEFAULT, props)?;
            img.ramdisk = rd.encode()?;
            let bytes = img.to_bytes()?;
            write(
                &out,
                &img,
                &avb::footed(&bytes, "recovery", size, &Sha256::digest(&bytes))?,
            )
        }
        Cmd::VendorRecovery {
            stock,
            binary,
            system_init,
            config,
            font,
            mono,
            mono_license,
            out,
        } => vendor_recovery(
            &stock,
            &binary,
            &system_init,
            &config,
            &font,
            &mono,
            &mono_license,
            &out,
        ),
        Cmd::Stock { config, out } => {
            let stock = skyward_config::DeviceConfig::load(&config)?
                .stock
                .with_context(|| format!("{} names no stock build", config.display()))?;
            println!(
                "version={}\npackage={}\nrecovery={}",
                stock.version, stock.package, stock.recovery
            );
            match out {
                Some(out) => take_stock_recovery(&stock, &out),
                None => Ok(()),
            }
        }
        Cmd::Described {
            config,
            partitions,
            out,
        } => {
            let stock = skyward_config::DeviceConfig::load(&config)?
                .stock
                .with_context(|| format!("{} names no stock build", config.display()))?;
            let partitions = if partitions.is_empty() {
                skyward_programs::phone::PARTITIONS
                    .map(str::to_string)
                    .to_vec()
            } else {
                partitions
            };
            described::take(&stock.package, &partitions, &out)
        }
    }
}

/// The partition a recovery is made on, as a full update package names it.
const RECOVERY: &str = "recovery";

/// Take the recovery partition out of the stock build's full update package
/// on its server, reading only what that partition takes of it, and write it
/// to `out` once its SHA-256 is the one the configuration names.
fn take_stock_recovery(stock: &skyward_config::Stock, out: &Path) -> Result<()> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("make {}", parent.display()))?;
    // Taken out beside where it goes, so it is moved there whole.
    let work = tempfile::Builder::new()
        .prefix(".stock")
        .tempdir_in(parent)
        .with_context(|| format!("make a folder in {}", parent.display()))?;
    let payload = unpayload::Payload::open_url(&stock.package)
        .with_context(|| format!("open {}", stock.package))?;
    let options = unpayload::ExtractOptions {
        output_dir: work.path().to_path_buf(),
        partitions: vec![RECOVERY.into()],
        ..Default::default()
    };
    let report = |event: unpayload::Event<'_>| match event {
        unpayload::Event::Started { partitions } => {
            for (name, bytes) in partitions {
                println!("taking {name} out of the package: {bytes} bytes");
            }
        }
        unpayload::Event::Extracted(image) => {
            println!("took {} out: {} bytes", image.partition, image.size)
        }
        unpayload::Event::Progress { .. } => {}
    };
    let taken = payload
        .extract(&options, &report)
        .with_context(|| format!("take {RECOVERY} out of {}", stock.package))?;
    let image = taken
        .iter()
        .find(|i| i.partition == RECOVERY)
        .with_context(|| format!("{} holds no {RECOVERY} partition", stock.package))?;
    let sum = hex::encode(
        image
            .sha256
            .as_deref()
            .context("the package's own sum of the image was not checked")?,
    );
    ensure!(
        sum == stock.recovery,
        "the {RECOVERY} partition in {} has the SHA-256 {sum}, where the configuration names {}",
        stock.package,
        stock.recovery
    );
    std::fs::rename(&image.path, out)
        .with_context(|| format!("move {} to {}", image.path.display(), out.display()))?;
    println!(
        "wrote {} ({} bytes), its SHA-256 the one the configuration names",
        out.display(),
        image.size
    );
    Ok(())
}

fn vendor_recovery(
    stock: &Path,
    binary: &Path,
    system_init: &Path,
    config: &Path,
    font: &Path,
    mono: &Path,
    mono_license: &Path,
    out: &Path,
) -> Result<()> {
    let mut img = VendorBoot::read(stock)?;
    let platform = img
        .fragments()
        .iter()
        .find(|f| f.kind == 1 && f.name().is_empty())
        .context("stock vendor_boot has no PLATFORM fragment")?
        .data
        .clone();
    let recovery = img
        .fragments()
        .iter()
        .find(|f| f.is_recovery())
        .context("stock vendor_boot has no recovery fragment")?
        .data
        .clone();
    let mut platform_rd = Ramdisk::decode(&platform)?;
    ensure!(
        platform_rd.find(paths::RECOVERY).is_some(),
        "platform fragment must contain system/bin/recovery as its init entry point"
    );
    platform_rd.remove_tree("res")?;
    platform_rd.remove_tree(paths::RECOVERY)?;
    img.replace_named(1, "", platform_rd.encode()?)?;
    let mut rd = Ramdisk::decode(&recovery)?;
    // Retain the stock RECOVERY overlay except for files it duplicates
    // byte-for-byte from PLATFORM; those files remain available in rootfs.
    let platform_files: std::collections::HashSet<(String, u32, Vec<u8>)> = platform_rd
        .entries
        .iter()
        .filter(|e| e.mode & 0o170000 == 0o100000)
        .map(|e| (e.name.clone(), e.mode, e.data.clone()))
        .collect();
    rd.entries.retain(|e| {
        !(e.mode & 0o170000 == 0o100000
            && platform_files.contains(&(e.name.clone(), e.mode, e.data.clone())))
    });
    let cfg = skyward_config::DeviceConfig::load(config)?;
    ensure!(
        cfg.device.codename == "turner",
        "vendor-recovery currently supports only the verified turner MT6991 layout"
    );
    let recovery_binary =
        std::fs::read(binary).with_context(|| format!("read {}", binary.display()))?;
    if rd.find(paths::RECOVERY).is_some() {
        rd.replace(paths::RECOVERY, recovery_binary)?;
    } else {
        rd.add(paths::RECOVERY, 0o755, recovery_binary)?;
    }
    let command = paths::RECOVERY
        .rsplit_once('/')
        .context("recovery binary path has no parent")?
        .1;
    for name in paths::COMMANDS {
        rd.link(name, command)?;
    }
    rd.add(
        paths::SYSTEM_INIT,
        0o755,
        std::fs::read(system_init).with_context(|| format!("read {}", system_init.display()))?,
    )?;
    rd.add(
        paths::CONFIG,
        0o644,
        std::fs::read(config).with_context(|| format!("read {}", config.display()))?,
    )?;
    rd.add(
        paths::FONT,
        0o644,
        std::fs::read(font).with_context(|| format!("read {}", font.display()))?,
    )?;
    rd.add(
        paths::MONO,
        0o644,
        std::fs::read(mono).with_context(|| format!("read {}", mono.display()))?,
    )?;
    rd.add(
        paths::MONO_LICENSE,
        0o644,
        std::fs::read(mono_license).with_context(|| format!("read {}", mono_license.display()))?,
    )?;
    if rd.find(paths::TASK_PROFILES).is_none() {
        rd.add(paths::TASK_PROFILES, 0o644, TASK_PROFILES.to_vec())?;
    }
    rd.add(paths::MTP_RC, 0o644, MTP_RC.to_vec())?;
    // The user-supplied working turner recovery carries this Binder ABI
    // compatibility shim and a non-isolated recovery linker namespace.
    rd.add(
        "skyward/libkmsglog.so",
        0o644,
        include_bytes!("../../../devices/turner/libkmsglog.so").to_vec(),
    )?;
    // The supplicant of this Android generation ends by itself when the service
    // directory will not take its AIDL interface, which the recovery has no
    // framework for. The WLAN here is driven over its control socket, so a shim
    // answers that one call; its source is devices/turner/wpaaidl.c.
    rd.add(
        "skyward/libwpaaidl.so",
        0o755,
        include_bytes!("../../../devices/turner/libwpaaidl.so").to_vec(),
    )?;
    // The symbol table the WLAN driver needs, rebuilt from KallRecon (GPL-2.0-only,
    // dere3046) with the kretprobe re-arm fix: the probe flags are cleared before
    // the hooks are armed again, without which the second arming stays disabled
    // while reporting success. The copy in the stock recovery does not carry it.
    let kallrecon = include_bytes!("../../../devices/turner/kallrecon.ko").to_vec();
    if rd.find("skyward/kallrecon.ko").is_some() {
        rd.replace("skyward/kallrecon.ko", kallrecon)?;
    } else {
        rd.add("skyward/kallrecon.ko", 0o644, kallrecon)?;
    }
    let linker_config = include_bytes!("../../../devices/turner/ld.config.txt").to_vec();
    if rd.find("system/etc/ld.config.txt").is_some() {
        rd.replace("system/etc/ld.config.txt", linker_config)?;
    } else {
        rd.add("system/etc/ld.config.txt", 0o644, linker_config)?;
    }
    rd.remove_tree("res")?;
    let init_path = "init.recovery.mt6991.rc";
    if let Some(entry) = rd.find(init_path) {
        let mut init = entry.data.clone();
        init.extend_from_slice(b"\n# Skyward turner recovery setup\non boot\n    setprop sys.usb.config adb\n    mkdir /data/vendor 0771 system system\n    mkdir /data/vendor/touch 0771 system system\n    mkdir /data/vendor/touch_state 0771 system system\n    mkdir /data/vendor/mqsas_common 0771 system system\n    mkdir /data/vendor/bsplog 0775 system system\n    mkdir /data/vendor/bsplog/touch 0775 system system\n    setprop vendor.teefs_state ready\n");
        rd.replace(init_path, init)?;
    }
    let stock_props = &rd
        .find(paths::PROP_DEFAULT)
        .context("stock recovery has no prop.default")?
        .data;
    let mut props = without(stock_props, &BUILD_VERSION);
    props.extend_from_slice(
        b"\nro.secure=0\nro.adb.secure=0\nro.debuggable=1\nro.ota.allow_downgrade=true\n",
    );
    rd.replace(paths::PROP_DEFAULT, props)?;
    img.replace_recovery(rd.encode()?)?;
    let body = img.encode()?;
    let stock_bytes = std::fs::read(stock).with_context(|| format!("read {}", stock.display()))?;
    let footer_at = stock_bytes
        .len()
        .checked_sub(64)
        .context("vendor_boot has no AVB footer")?;
    ensure!(
        &stock_bytes[footer_at..footer_at + 4] == b"AVBf",
        "stock vendor_boot has no AVB footer"
    );
    let mut salt = [0u8; 32];
    let footer = &stock_bytes[footer_at..];
    let vbmeta_offset = usize::try_from(u64::from_be_bytes(footer[20..28].try_into()?))?;
    let vbmeta_size = usize::try_from(u64::from_be_bytes(footer[28..36].try_into()?))?;
    ensure!(
        vbmeta_offset >= usize::try_from(u64::from_be_bytes(footer[12..20].try_into()?))?
            && vbmeta_size >= 256
            && vbmeta_offset
                .checked_add(vbmeta_size)
                .is_some_and(|n| n <= footer_at),
        "invalid stock AVB footer"
    );
    let vbmeta_end = vbmeta_offset
        .checked_add(vbmeta_size)
        .context("AVB vbmeta range overflow")?;
    let vbmeta = stock_bytes
        .get(vbmeta_offset..vbmeta_end)
        .context("stock vbmeta range is invalid")?;
    ensure!(
        vbmeta.len() >= 256 && vbmeta.get(..4) == Some(b"AVB0".as_slice()),
        "invalid vendor_boot vbmeta image"
    );
    let auth_size = usize::try_from(u64::from_be_bytes(vbmeta[12..20].try_into()?))?;
    let aux_size = usize::try_from(u64::from_be_bytes(vbmeta[20..28].try_into()?))?;
    ensure!(
        256usize
            .checked_add(auth_size)
            .and_then(|n| n.checked_add(aux_size))
            == Some(vbmeta.len()),
        "vendor_boot vbmeta sizes do not match"
    );
    let desc_offset = usize::try_from(u64::from_be_bytes(vbmeta[96..104].try_into()?))?;
    let desc_size = usize::try_from(u64::from_be_bytes(vbmeta[104..112].try_into()?))?;
    let desc_at = 256usize
        .checked_add(auth_size)
        .and_then(|n| n.checked_add(desc_offset))
        .context("AVB descriptor offset overflow")?;
    ensure!(
        desc_offset
            .checked_add(desc_size)
            .is_some_and(|n| n <= aux_size),
        "AVB descriptor exceeds auxiliary block"
    );
    ensure!(
        vbmeta
            .get(
                desc_at
                    ..desc_at
                        .checked_add(desc_size)
                        .context("AVB descriptor range overflow")?
            )
            .is_some(),
        "AVB descriptor range is invalid"
    );
    let desc_end = desc_at
        .checked_add(desc_size)
        .context("AVB descriptor range overflow")?;
    let desc = vbmeta
        .get(desc_at..desc_end)
        .context("AVB descriptor range is invalid")?;
    ensure!(
        desc.len() >= 132 && u64::from_be_bytes(desc[..8].try_into()?) == 2,
        "vendor_boot AVB has no hash descriptor"
    );
    let name_len = usize::try_from(u32::from_be_bytes(desc[56..60].try_into()?))?;
    let salt_len = usize::try_from(u32::from_be_bytes(desc[60..64].try_into()?))?;
    let salt_at = 132usize
        .checked_add(name_len)
        .context("AVB name range overflow")?;
    ensure!(
        desc.get(132..salt_at) == Some(b"vendor_boot".as_slice()),
        "AVB descriptor is not for vendor_boot"
    );
    ensure!(
        salt_len == salt.len(),
        "expected a 32-byte stock AVB salt, found {salt_len}"
    );
    let salt_end = salt_at
        .checked_add(salt_len)
        .context("AVB salt range overflow")?;
    salt.copy_from_slice(
        desc.get(salt_at..salt_end)
            .context("stock AVB salt is truncated")?,
    );
    let image = avb::footed(&body, "vendor_boot", stock_bytes.len(), &salt)?;
    std::fs::write(out, &image).with_context(|| format!("write {}", out.display()))?;
    println!(
        "wrote {} ({} bytes vendor_boot, {} recovery ramdisk bytes, {} bytes total)",
        out.display(),
        body.len(),
        recovery.len(),
        image.len()
    );
    Ok(())
}

/// The size of the partition `stock` fills: all of it, as the phone and a
/// full update package hold it, which its AVB footer ends.
fn partition_size(stock: &Path) -> Result<usize> {
    let mut file = File::open(stock).with_context(|| format!("open {}", stock.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", stock.display()))?
        .len();
    let mut magic = [0u8; 4];
    if size >= 64 {
        file.seek(SeekFrom::Start(size - 64))
            .with_context(|| format!("seek in {}", stock.display()))?;
        file.read_exact(&mut magic)
            .with_context(|| format!("read {}", stock.display()))?;
    }
    ensure!(
        &magic == b"AVBf",
        "{} does not end in an AVB footer; give the whole recovery partition, as the phone or a full update package holds it",
        stock.display()
    );
    Ok(usize::try_from(size)?)
}

/// What a recovery image says about the build it is: the version and patch
/// levels of its own ramdisk's system and vendor partition, which are older
/// than those installed beside it whenever the phone has taken an update.
/// The phone's secure services read these to tell the secure world which
/// build is running, and keys the installed system made are refused when
/// they say the wrong one, so they are left out here for the recovery to
/// fill in from the installed system once it has that mounted.
const BUILD_VERSION: [&str; 3] = [
    "ro.build.version.release",
    "ro.build.version.security_patch",
    "ro.vendor.build.security_patch",
];

/// The scheduling profiles a recovery's programs look up by name. A
/// recovery's cgroup setup mounts no controllers, so of all a system's
/// profiles do, only the timer slack applies here.
const TASK_PROFILES: &[u8] = include_bytes!("task_profiles.json");

/// The init rules that give the USB port file transfer beside adb, which a
/// stock recovery does not offer.
const MTP_RC: &[u8] = include_bytes!("mtp.rc");

/// A property file without the lines that set any of `names`.
fn without(props: &[u8], names: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(props.len());
    for line in props.split_inclusive(|b| *b == b'\n') {
        let key = line.split(|b| *b == b'=').next().unwrap_or_default();
        if !names.iter().any(|n| n.as_bytes() == key) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Write `bytes`, the image `img` makes, to `out`.
fn write(out: &Path, img: &BootImage, bytes: &[u8]) -> Result<()> {
    std::fs::write(out, bytes).with_context(|| format!("write {}", out.display()))?;
    println!(
        "wrote {} ({} bytes kernel, {} bytes ramdisk, {} bytes in all)",
        out.display(),
        img.kernel.len(),
        img.ramdisk.len(),
        bytes.len()
    );
    Ok(())
}

fn list(image: &Path) -> Result<()> {
    let img = BootImage::read(image)?;
    let h = &img.header;
    println!(
        "header v{}  kernel {} bytes  ramdisk {} bytes  {:?}",
        h.header_version(),
        img.kernel.len(),
        img.ramdisk.len(),
        h.osversionpatch
    );
    if img.ramdisk.is_empty() {
        return Ok(());
    }
    let rd = Ramdisk::decode(&img.ramdisk)?;
    for e in &rd.entries {
        println!(
            "{:06o} {:>5}:{:<5} {:>10}  {}",
            e.mode,
            e.uid,
            e.gid,
            e.data.len(),
            e.name
        );
    }
    println!("{} entries", rd.entries.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_that_set_a_name_are_the_only_ones_left_out() {
        let props = b"ro.build.id=TQ\nro.build.version.release=13\nro.build.version.release_or_codename=13\nro.product.name=ishtar\n\
ro.vendor.build.security_patch=2026-10-01\nro.vendor.build.security_patch_level=1\n";
        let out = without(props, &BUILD_VERSION);
        let text = String::from_utf8(out).expect("property files are text");
        assert!(!text.contains("ro.build.version.release=13"));
        assert!(
            !text.contains("ro.vendor.build.security_patch=2026-10-01"),
            "the vendor partition's patch level is left out too"
        );
        assert!(
            text.contains("ro.build.version.release_or_codename=13"),
            "a longer name is left alone"
        );
        assert!(
            text.contains("ro.vendor.build.security_patch_level=1"),
            "a longer name is left alone"
        );
        assert!(text.contains("ro.build.id=TQ") && text.contains("ro.product.name=ishtar"));
    }

    #[test]
    fn the_profiles_name_all_the_recovery_asks_for() {
        let profiles: serde_json::Value =
            serde_json::from_slice(TASK_PROFILES).expect("the profiles are JSON");
        let names = |key: &str| -> Vec<String> {
            profiles[key]
                .as_array()
                .expect("a list")
                .iter()
                .map(|p| p["Name"].as_str().expect("a name").to_string())
                .collect()
        };
        let (plain, aggregate) = (names("Profiles"), names("AggregateProfiles"));
        // What libprocessgroup asks for when a program sets its scheduling
        // policy, and what the recovery's own init scripts give logd.
        let asked = [
            "SCHED_SP_DEFAULT",
            "SCHED_SP_BACKGROUND",
            "SCHED_SP_FOREGROUND",
            "SCHED_SP_TOP_APP",
            "SCHED_SP_SYSTEM",
            "SCHED_SP_RT_APP",
            "CPUSET_SP_DEFAULT",
            "CPUSET_SP_BACKGROUND",
            "CPUSET_SP_FOREGROUND",
            "CPUSET_SP_TOP_APP",
            "CPUSET_SP_SYSTEM",
            "CPUSET_SP_RESTRICTED",
            "BlkIOBackground",
            "BlkIOForeground",
            "ServiceCapacityLow",
        ];
        for name in asked {
            assert!(
                plain.iter().chain(&aggregate).any(|n| n == name),
                "{name} is missing"
            );
        }
        for a in profiles["AggregateProfiles"].as_array().expect("a list") {
            for part in a["Profiles"].as_array().expect("a list") {
                let part = part.as_str().expect("a name");
                assert!(
                    plain.iter().any(|n| n == part),
                    "{} is made of {part}, which is not a profile",
                    a["Name"]
                );
            }
        }
    }

    #[test]
    fn the_partition_is_as_large_as_a_stock_image_ending_in_its_footer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.img");
        let mut whole = vec![0u8; 8192];
        whole[8192 - 64..8192 - 60].copy_from_slice(b"AVBf");
        std::fs::write(&path, &whole).unwrap();
        assert_eq!(partition_size(&path).unwrap(), 8192);
        std::fs::write(&path, &whole[..8000]).unwrap();
        assert!(
            partition_size(&path).is_err(),
            "an image cut short of its partition's end"
        );
        std::fs::write(&path, b"AVBf").unwrap();
        assert!(
            partition_size(&path).is_err(),
            "too short to end in a footer"
        );
    }

    #[test]
    fn a_file_without_a_last_newline_keeps_its_last_line() {
        let out = without(b"a=1\nro.build.version.release=13", &BUILD_VERSION);
        assert_eq!(out, b"a=1\n");
        let kept = without(b"a=1\nb=2", &BUILD_VERSION);
        assert_eq!(kept, b"a=1\nb=2");
    }
}
