// SPDX-License-Identifier: GPL-3.0-or-later

//! A K-Touch release zip built the way the HelixScreen release builds it: `flash_args`, the five
//! parts it lists and the merged factory image, under one top-level folder.

#![allow(dead_code)]

use std::io::Write;

pub const APP_OFFSET: u32 = 0x20000;
pub const FLASH_ARGS: &str = "--flash_mode dio --flash_freq 80m --flash_size 16MB\n\
0x0 bootloader/bootloader.bin\n\
0x20000 helixscreen_esp32.bin\n\
0x8000 partition_table/partition-table.bin\n\
0xf000 ota_data_initial.bin\n\
0xd20000 storage_frogfs.bin\n";

/// The size of the merged factory image in the zip, which no write may have.
pub const FACTORY_LEN: usize = 5000;

/// The offsets `FLASH_ARGS` writes, lowest first.
pub const PART_OFFSETS: [u32; 5] = [0x0, 0x8000, 0xf000, 0x20000, 0xd20000];

/// An app image whose descriptor names `project` and `version`.
pub fn app_image(project: &str, version: &str) -> Vec<u8> {
    let mut img = vec![0u8; 0x1000];
    img[..4].copy_from_slice(&[0xe9, 0x06, 0x02, 0x4f]);
    img[0x20..0x24].copy_from_slice(&0xABCD5432u32.to_le_bytes());
    img[0x30..0x30 + version.len()].copy_from_slice(version.as_bytes());
    img[0x50..0x50 + project.len()].copy_from_slice(project.as_bytes());
    for (i, b) in img.iter_mut().enumerate().skip(0x120) {
        *b = (i % 251) as u8;
    }
    img
}

pub fn bootloader() -> Vec<u8> {
    let mut b = vec![0x5au8; 1024];
    b[..4].copy_from_slice(&[0xe9, 0x03, 0x02, 0x4f]);
    b
}

fn entry(kind: u8, subtype: u8, offset: u32, size: u32, label: &str) -> Vec<u8> {
    let mut e = vec![0xaa, 0x50, kind, subtype];
    e.extend(offset.to_le_bytes());
    e.extend(size.to_le_bytes());
    let mut name = [0u8; 16];
    name[..label.len()].copy_from_slice(label.as_bytes());
    e.extend(name);
    e.extend([0u8; 4]);
    e
}

/// The K-Touch's partition table: WiFi in `nvs` at 0x9000, settings in `cfg` at 0xfe0000.
pub fn partition_table() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend(entry(1, 2, 0x9000, 0x6000, "nvs"));
    t.extend(entry(1, 0, 0xf000, 0x2000, "otadata"));
    t.extend(entry(1, 1, 0x11000, 0x1000, "phy_init"));
    t.extend(entry(0, 0x10, 0x20000, 0x680000, "ota_0"));
    t.extend(entry(0, 0x11, 0x6a0000, 0x680000, "ota_1"));
    t.extend(entry(1, 0x82, 0xd20000, 0x2c0000, "storage"));
    t.extend(entry(1, 0x82, 0xfe0000, 0x20000, "cfg"));
    t.extend([0xeb, 0xeb]);
    t.resize(3072, 0xff);
    t
}

/// A release zip for `tag` whose app reports `app_version`, with `flash_args` as given.
pub fn zip_with(tag: &str, app_version: &str, flash_args: &str) -> Vec<u8> {
    let dir = format!("helixscreen-esp32-ktouch-{tag}");
    let files: Vec<(String, Vec<u8>)> = vec![
        ("flash_args".into(), flash_args.as_bytes().to_vec()),
        ("README.txt".into(), b"HelixScreen for the K-Touch".to_vec()),
        ("bootloader/bootloader.bin".into(), bootloader()),
        (
            "helixscreen_esp32.bin".into(),
            app_image("helixscreen_esp32", app_version),
        ),
        (
            "partition_table/partition-table.bin".into(),
            partition_table(),
        ),
        ("ota_data_initial.bin".into(), vec![0xff; 8192]),
        ("storage_frogfs.bin".into(), vec![0x33; 2000]),
        (
            "helixscreen-esp32-ktouch-factory.bin".into(),
            vec![0x77; FACTORY_LEN],
        ),
    ];
    let mut out = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        zip.start_file(format!("{dir}/{name}"), opts).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
    out.into_inner()
}

/// The release zip for `tag` (`v1.1.0-beta.10`), its app reporting that version.
pub fn release_zip(tag: &str) -> Vec<u8> {
    zip_with(tag, tag, FLASH_ARGS)
}
