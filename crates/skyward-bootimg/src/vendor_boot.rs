use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::Path;

const MAGIC: &[u8; 8] = b"VNDRBOOT";
const HEADER_V4_MIN: usize = 2128;
const FOOTER_LEN: usize = 64;
const MAGIC_DTB_TABLE: u32 = 0xd7b7ab1e;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VendorRamdisk {
    pub data: Vec<u8>,
    pub kind: u32,
    pub name: [u8; 32],
    pub board_id: [u32; 16],
    reserved: Vec<u8>,
}

impl VendorRamdisk {
    pub fn name(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.name.len());
        std::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn is_recovery(&self) -> bool {
        self.kind == 2 && self.name() == "recovery"
    }
}

#[derive(Clone, Debug)]
pub struct VendorBoot {
    header: Vec<u8>,
    page_size: usize,
    header_size: usize,
    table_entry_size: usize,
    descriptors: Vec<VendorRamdisk>,
    dtb: Vec<u8>,
    bootconfig: Vec<u8>,
    partition_size: usize,
    original_body: Vec<u8>,
    original_descriptors: Vec<VendorRamdisk>,
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .context("vendor_boot header is truncated")?
            .try_into()?,
    ))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) -> Result<()> {
    bytes
        .get_mut(at..at + 4)
        .context("vendor_boot header is truncated")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn aligned(value: usize, page: usize) -> Result<usize> {
    value
        .checked_add(page - 1)
        .context("vendor_boot alignment overflow")
        .map(|n| n / page * page)
}

fn range(bytes: &[u8], at: usize, len: usize, what: &str) -> Result<Vec<u8>> {
    let end = at
        .checked_add(len)
        .with_context(|| format!("{what} range overflow"))?;
    Ok(bytes
        .get(at..end)
        .with_context(|| format!("{what} runs past the end of vendor_boot"))?
        .to_vec())
}

impl VendorBoot {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER_V4_MIN + FOOTER_LEN,
            "vendor_boot image is too short"
        );
        ensure!(&bytes[..8] == MAGIC, "not a vendor_boot image");
        let version = read_u32(bytes, 8)?;
        ensure!(
            version == 4,
            "vendor_boot header version {version} is not v4"
        );
        let page_size = usize::try_from(read_u32(bytes, 12)?)?;
        ensure!(
            page_size >= 512 && page_size.is_power_of_two(),
            "invalid vendor_boot page size {page_size}"
        );
        let ramdisk_size = usize::try_from(read_u32(bytes, 24)?)?;
        let header_size = usize::try_from(read_u32(bytes, 2096)?)?;
        ensure!(
            header_size >= HEADER_V4_MIN && header_size <= page_size,
            "invalid vendor_boot header size {header_size}"
        );
        let dtb_size = usize::try_from(read_u32(bytes, 2100)?)?;
        let table_size = usize::try_from(read_u32(bytes, 2112)?)?;
        let count = usize::try_from(read_u32(bytes, 2116)?)?;
        let table_entry_size = usize::try_from(read_u32(bytes, 2120)?)?;
        let bootconfig_size = usize::try_from(read_u32(bytes, 2124)?)?;
        ensure!(
            table_entry_size >= 108,
            "vendor ramdisk table entry is too short"
        );
        ensure!(
            count.checked_mul(table_entry_size) == Some(table_size),
            "vendor ramdisk table dimensions do not match"
        );
        let partition_size = bytes.len();
        let avb_at = partition_size - FOOTER_LEN;
        ensure!(
            &bytes[avb_at..avb_at + 4] == b"AVBf",
            "vendor_boot does not end with an AVB footer"
        );
        let original_size = usize::try_from(u64::from_be_bytes(
            bytes[avb_at + 12..avb_at + 20].try_into()?,
        ))?;
        let vbmeta_offset = usize::try_from(u64::from_be_bytes(
            bytes[avb_at + 20..avb_at + 28].try_into()?,
        ))?;
        let vbmeta_size = usize::try_from(u64::from_be_bytes(
            bytes[avb_at + 28..avb_at + 36].try_into()?,
        ))?;
        ensure!(
            original_size <= vbmeta_offset
                && vbmeta_offset
                    .checked_add(vbmeta_size)
                    .is_some_and(|end| end <= avb_at),
            "invalid vendor_boot AVB footer ranges"
        );
        ensure!(
            vbmeta_size >= 256
                && bytes.get(vbmeta_offset..vbmeta_offset + 4) == Some(b"AVB0".as_slice()),
            "vendor_boot AVB footer does not locate a vbmeta image"
        );

        let header = range(bytes, 0, header_size, "header")?;
        let ramdisk_at = aligned(page_size, page_size)?;
        ensure!(
            ramdisk_at <= original_size,
            "ramdisk begins past image content"
        );
        let ramdisk_end = ramdisk_at
            .checked_add(ramdisk_size)
            .context("vendor ramdisk size overflow")?;
        let dtb_at = aligned(ramdisk_end, page_size)?;
        let dtb_end = dtb_at
            .checked_add(dtb_size)
            .context("vendor DTB size overflow")?;
        let table_at = aligned(dtb_end, page_size)?;
        let table_end = table_at
            .checked_add(table_size)
            .context("vendor table size overflow")?;
        let bootconfig_at = aligned(table_end, page_size)?;
        let bootconfig_end = bootconfig_at
            .checked_add(bootconfig_size)
            .context("bootconfig size overflow")?;
        ensure!(
            bootconfig_end <= original_size,
            "vendor_boot sections run past AVB image size"
        );
        let table = range(bytes, table_at, table_size, "vendor ramdisk table")?;
        let mut descriptors = Vec::with_capacity(count);
        let mut expected_offset = 0usize;
        for i in 0..count {
            let at = i
                .checked_mul(table_entry_size)
                .context("table index overflow")?;
            let size = usize::try_from(read_u32(&table, at)?)?;
            let offset = usize::try_from(read_u32(&table, at + 4)?)?;
            let kind = read_u32(&table, at + 8)?;
            let data_at = ramdisk_at
                .checked_add(offset)
                .context("ramdisk fragment offset overflow")?;
            ensure!(
                offset == expected_offset,
                "vendor ramdisk fragment {i} is not contiguous in table order"
            );
            expected_offset = offset
                .checked_add(size)
                .context("vendor ramdisk fragment end overflow")?;
            ensure!(
                expected_offset <= ramdisk_size,
                "vendor ramdisk fragment {i} exceeds ramdisk section"
            );
            let mut name = [0u8; 32];
            name.copy_from_slice(&table[at + 12..at + 44]);
            let mut board_id = [0u32; 16];
            for (j, value) in board_id.iter_mut().enumerate() {
                *value = read_u32(&table, at + 44 + j * 4)?;
            }
            let reserved = table[at + 108..at + table_entry_size].to_vec();
            descriptors.push(VendorRamdisk {
                data: range(bytes, data_at, size, "vendor ramdisk fragment")?,
                kind,
                name,
                board_id,
                reserved,
            });
        }
        ensure!(
            expected_offset == ramdisk_size,
            "vendor ramdisk table does not cover its section"
        );
        let dtb = range(bytes, dtb_at, dtb_size, "DTB")?;
        if dtb.len() >= 32 && u32::from_be_bytes(dtb[..4].try_into()?) == MAGIC_DTB_TABLE {
            let total = usize::try_from(u32::from_be_bytes(dtb[4..8].try_into()?))?;
            let header_bytes = usize::try_from(u32::from_be_bytes(dtb[8..12].try_into()?))?;
            let entry_size = usize::try_from(u32::from_be_bytes(dtb[12..16].try_into()?))?;
            let entries = usize::try_from(u32::from_be_bytes(dtb[16..20].try_into()?))?;
            let entries_at = usize::try_from(u32::from_be_bytes(dtb[20..24].try_into()?))?;
            ensure!(
                total == dtb.len()
                    && header_bytes >= 32
                    && entry_size >= 32
                    && entries_at >= header_bytes
                    && entries
                        .checked_mul(entry_size)
                        .and_then(|n| n.checked_add(entries_at))
                        .is_some_and(|n| n <= total),
                "invalid Android DT table"
            );
        }
        let bootconfig = range(bytes, bootconfig_at, bootconfig_size, "bootconfig")?;
        let original_body = range(bytes, 0, original_size, "vendor_boot body")?;
        let original_descriptors = descriptors.clone();
        Ok(Self {
            header,
            page_size,
            header_size,
            table_entry_size,
            descriptors,
            dtb,
            bootconfig,
            partition_size,
            original_body,
            original_descriptors,
        })
    }

    pub fn fragments(&self) -> &[VendorRamdisk] {
        &self.descriptors
    }

    pub fn partition_size(&self) -> usize {
        self.partition_size
    }

    pub fn body(&self) -> &[u8] {
        &self.original_body
    }

    pub fn replace_named(&mut self, kind: u32, name: &str, data: Vec<u8>) -> Result<()> {
        let mut matches = self
            .descriptors
            .iter_mut()
            .filter(|f| f.kind == kind && f.name() == name);
        let fragment = matches
            .next()
            .with_context(|| format!("no vendor ramdisk fragment type {kind} named {name}"))?;
        ensure!(
            matches.next().is_none(),
            "multiple vendor ramdisk fragments type {kind} named {name}"
        );
        fragment.data = data;
        Ok(())
    }

    pub fn replace_recovery(&mut self, data: Vec<u8>) -> Result<()> {
        let mut found = self
            .descriptors
            .iter_mut()
            .filter(|fragment| fragment.is_recovery());
        let fragment = found
            .next()
            .context("vendor_boot has no RECOVERY fragment named recovery")?;
        ensure!(
            found.next().is_none(),
            "vendor_boot has multiple recovery fragments"
        );
        fragment.data = data;
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.descriptors == self.original_descriptors {
            return Ok(self.original_body.clone());
        }
        let mut header = self.header.clone();
        let ramdisk_size = self.descriptors.iter().try_fold(0usize, |n, f| {
            n.checked_add(f.data.len())
                .context("vendor ramdisk total size overflow")
        })?;
        ensure!(
            ramdisk_size <= u32::MAX as usize,
            "vendor ramdisk is too large"
        );
        put_u32(
            &mut header,
            24,
            u32::try_from(ramdisk_size).context("vendor ramdisk is too large")?,
        )?;
        let table_size = self
            .descriptors
            .len()
            .checked_mul(self.table_entry_size)
            .context("vendor ramdisk table size overflow")?;
        put_u32(&mut header, 2112, u32::try_from(table_size)?)?;
        put_u32(&mut header, 2116, u32::try_from(self.descriptors.len())?)?;
        put_u32(&mut header, 2120, u32::try_from(self.table_entry_size)?)?;
        let mut out = header;
        out.resize(aligned(self.header_size, self.page_size)?, 0);
        let mut table = Vec::with_capacity(table_size);
        let mut ramdisk_offset = 0usize;
        for fragment in &self.descriptors {
            let size = u32::try_from(fragment.data.len())
                .context("vendor ramdisk fragment is too large")?;
            let offset =
                u32::try_from(ramdisk_offset).context("vendor ramdisk offset is too large")?;
            table.extend_from_slice(&size.to_le_bytes());
            table.extend_from_slice(&offset.to_le_bytes());
            table.extend_from_slice(&fragment.kind.to_le_bytes());
            table.extend_from_slice(&fragment.name);
            for id in fragment.board_id {
                table.extend_from_slice(&id.to_le_bytes());
            }
            table.extend_from_slice(&fragment.reserved);
            out.extend_from_slice(&fragment.data);
            ramdisk_offset = ramdisk_offset
                .checked_add(fragment.data.len())
                .context("vendor ramdisk total size overflow")?;
        }
        out.resize(aligned(out.len(), self.page_size)?, 0);
        out.extend_from_slice(&self.dtb);
        out.resize(aligned(out.len(), self.page_size)?, 0);
        out.extend_from_slice(&table);
        out.resize(aligned(out.len(), self.page_size)?, 0);
        out.extend_from_slice(&self.bootconfig);
        ensure!(
            out.len()
                .checked_add(4096 + FOOTER_LEN)
                .is_some_and(|n| n <= self.partition_size),
            "vendor_boot content and AVB footer do not fit {}-byte partition",
            self.partition_size
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_non_vendor_boot_image() {
        assert!(VendorBoot::parse(&vec![0; 8192]).is_err());
    }

    #[test]
    #[ignore = "requires SKYWARD_VENDOR_BOOT_STOCK full partition fixture"]
    fn stock_roundtrip_and_recovery_replacement() {
        let bytes = fs::read(std::env::var("SKYWARD_VENDOR_BOOT_STOCK").unwrap()).unwrap();
        let mut image = VendorBoot::parse(&bytes).unwrap();
        assert_eq!(image.encode().unwrap(), image.body());
        let original = image.fragments().to_vec();
        let index = original
            .iter()
            .position(VendorRamdisk::is_recovery)
            .unwrap();
        let replacement = vec![0x42; 1024];
        image.replace_recovery(replacement.clone()).unwrap();
        let body = image.encode().unwrap();
        let vbmeta_at = (body.len() + 4095) / 4096 * 4096;
        let mut wrapped = body.clone();
        wrapped.resize(bytes.len(), 0);
        wrapped[vbmeta_at..vbmeta_at + 4].copy_from_slice(b"AVB0");
        let footer = wrapped.len() - 64;
        wrapped[footer..footer + 4].copy_from_slice(b"AVBf");
        for (offset, value) in [(12, body.len()), (20, vbmeta_at), (28, 256)] {
            wrapped[footer + offset..footer + offset + 8]
                .copy_from_slice(&(value as u64).to_be_bytes());
        }
        let decoded = VendorBoot::parse(&wrapped).unwrap();
        assert_eq!(decoded.dtb, image.dtb);
        assert_eq!(decoded.bootconfig, image.bootconfig);
        for (i, fragment) in decoded.fragments().iter().enumerate() {
            if i == index {
                assert_eq!(fragment.data, replacement);
                assert_eq!(fragment.board_id, original[i].board_id);
                assert_eq!(fragment.name, original[i].name);
            } else {
                assert_eq!(fragment, &original[i]);
            }
        }
    }

    #[test]
    fn checked_alignment_does_not_overflow() {
        assert!(aligned(usize::MAX, 4096).is_err());
        assert_eq!(aligned(4096, 4096).unwrap(), 4096);
    }
}
