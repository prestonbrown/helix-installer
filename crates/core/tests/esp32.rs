// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading a K-Touch release zip, an ESP32 app's descriptor and boot log, and the USB ports the
//! picker offers.

#[path = "support/ktouch.rs"]
mod ktouch;

use helix_installer_core::discovery::usb::{candidates, usb_id};
use helix_installer_core::steps::esp32::{
    app_desc, boot_version, check_image, permission_help, probe_facts, read_release_zip,
};
use helix_installer_core::transport::{ChipInfo, UsbPort};
use ktouch::*;
use serde_json::json;

const MIB16: u64 = 16 * 1024 * 1024;

fn keep() -> Vec<String> {
    vec!["nvs".into(), "cfg".into()]
}

fn s3() -> ChipInfo {
    ChipInfo {
        chip: "esp32s3".into(),
        flash_bytes: MIB16,
        mac: Some("dc:da:0c:11:22:33".into()),
    }
}

fn port(name: &str, vid: u16, pid: u16) -> UsbPort {
    UsbPort {
        name: name.into(),
        vid,
        pid,
    }
}

#[test]
fn the_app_descriptor_names_the_project_and_version() {
    let d = app_desc(&app_image("helixscreen_esp32", "v1.1.0-beta.10")).unwrap();
    assert_eq!(d.project, "helixscreen_esp32");
    assert_eq!(d.version, "v1.1.0-beta.10");
}

#[test]
fn erased_or_foreign_flash_has_no_app_descriptor() {
    assert!(app_desc(&[0xff; 0x120]).is_none());
    let mut img = app_image("helixscreen_esp32", "v1.1.0");
    img[0x20] = 0x00;
    assert!(app_desc(&img).is_none());
    assert!(app_desc(&img[..0x40]).is_none());
}

#[test]
fn a_release_zip_yields_exactly_the_flash_args_parts() {
    let img = read_release_zip(&release_zip("v1.1.0-beta.10")).unwrap();
    let offsets: Vec<u32> = img.parts.iter().map(|p| p.offset).collect();
    assert_eq!(offsets, PART_OFFSETS.to_vec());
    assert_eq!(img.flash_size, "16MB");
    assert_eq!(img.flash_mode, "dio");
    assert_eq!(img.flash_freq, "80m");
    assert!(img.parts.iter().all(|p| !p.name.contains("factory")));
    let app = img.parts.iter().find(|p| p.offset == APP_OFFSET).unwrap();
    assert_eq!(app.name, "helixscreen_esp32.bin");
    assert_eq!(app.data, app_image("helixscreen_esp32", "v1.1.0-beta.10"));
    assert_eq!(img.app().unwrap().version, "v1.1.0-beta.10");
}

#[test]
fn a_zip_without_flash_args_is_refused() {
    let args_free = zip_with("v1.1.0", "v1.1.0", "");
    let err = read_release_zip(&args_free).unwrap_err();
    assert!(err.contains("flash_args"), "{err}");
    let err = read_release_zip(b"not a zip").unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn flash_args_naming_a_missing_or_escaping_file_is_refused() {
    let missing = format!("{FLASH_ARGS}0x100000 nothing_here.bin\n");
    let err = read_release_zip(&zip_with("v1.1.0", "v1.1.0", &missing)).unwrap_err();
    assert!(err.contains("nothing_here.bin"), "{err}");
    let escaping = format!("{FLASH_ARGS}0x100000 ../outside.bin\n");
    assert!(read_release_zip(&zip_with("v1.1.0", "v1.1.0", &escaping)).is_err());
    let absolute = format!("{FLASH_ARGS}0x100000 /etc/passwd\n");
    assert!(read_release_zip(&zip_with("v1.1.0", "v1.1.0", &absolute)).is_err());
    let unreadable_offset = format!("{FLASH_ARGS}nowhere storage_frogfs.bin\n");
    assert!(read_release_zip(&zip_with("v1.1.0", "v1.1.0", &unreadable_offset)).is_err());
}

#[test]
fn flash_args_listing_the_factory_image_is_refused() {
    let args = "--flash_mode dio --flash_freq 80m --flash_size 16MB\n\
                0x0 helixscreen-esp32-ktouch-factory.bin\n";
    let err = read_release_zip(&zip_with("v1.1.0", "v1.1.0", args)).unwrap_err();
    assert!(err.contains("factory"), "{err}");
}

#[test]
fn the_k_touch_build_fits_a_16_mb_board_and_leaves_wifi_and_settings_alone() {
    let img = read_release_zip(&release_zip("v1.1.0")).unwrap();
    assert_eq!(check_image(&img, MIB16, &keep()), Ok(()));
}

#[test]
fn a_part_over_a_kept_partition_is_refused() {
    let over_nvs = format!("{FLASH_ARGS}0xa000 ota_data_initial.bin\n");
    let img = read_release_zip(&zip_with("v1.1.0", "v1.1.0", &over_nvs)).unwrap();
    let err = check_image(&img, MIB16, &keep()).unwrap_err();
    assert!(err.contains("nvs"), "{err}");
    let over_cfg = format!("{FLASH_ARGS}0xfe1000 ota_data_initial.bin\n");
    let img = read_release_zip(&zip_with("v1.1.0", "v1.1.0", &over_cfg)).unwrap();
    let err = check_image(&img, MIB16, &keep()).unwrap_err();
    assert!(err.contains("cfg"), "{err}");
}

#[test]
fn a_kept_partition_the_table_lacks_is_refused() {
    let img = read_release_zip(&release_zip("v1.1.0")).unwrap();
    let err = check_image(&img, MIB16, &["settings".to_string()]).unwrap_err();
    assert!(err.contains("settings"), "{err}");
}

#[test]
fn a_build_for_other_flash_is_refused() {
    let img = read_release_zip(&release_zip("v1.1.0")).unwrap();
    assert!(check_image(&img, 8 * 1024 * 1024, &keep()).is_err());
    let args = FLASH_ARGS.replace("16MB", "8MB");
    let img = read_release_zip(&zip_with("v1.1.0", "v1.1.0", &args)).unwrap();
    let err = check_image(&img, MIB16, &keep()).unwrap_err();
    assert!(err.contains("8MB"), "{err}");
}

#[test]
fn overlapping_parts_are_refused() {
    let args = format!("{FLASH_ARGS}0x20800 ota_data_initial.bin\n");
    let img = read_release_zip(&zip_with("v1.1.0", "v1.1.0", &args)).unwrap();
    let err = check_image(&img, MIB16, &keep()).unwrap_err();
    assert!(err.contains("overlap"), "{err}");
}

#[test]
fn a_bootloader_built_for_other_flash_settings_is_refused() {
    let mut img = read_release_zip(&release_zip("v1.1.0")).unwrap();
    assert_eq!(img.parts[0].offset, 0);
    img.parts[0].data[3] = 0x3f;
    let err = check_image(&img, MIB16, &keep()).unwrap_err();
    assert!(err.contains("bootloader"), "{err}");
    let mut img = read_release_zip(&release_zip("v1.1.0")).unwrap();
    img.parts[0].data[2] = 0x00;
    assert!(check_image(&img, MIB16, &keep()).is_err());
}

#[test]
fn the_boot_log_gives_the_version_the_app_started() {
    let log = "ESP-ROM:esp32s3-20210327\nI (310) app_init: Project name:     helixscreen_esp32\n\
               I (312) app_init: App version:      v1.1.0-beta.10\nI (318) app_init: Compile time: x\n";
    assert_eq!(boot_version(log).as_deref(), Some("v1.1.0-beta.10"));
    assert_eq!(boot_version("rst:0x1 (POWERON)\ninvalid header\n"), None);
    assert_eq!(boot_version("I (1) app_init: App version:\n"), None);
}

#[test]
fn a_board_running_helixscreen_reports_it_as_installed() {
    let f = probe_facts(&s3(), &app_image("helixscreen_esp32", "v1.1.0-beta.9"));
    assert_eq!(f["esp_chip"], json!("esp32s3"));
    assert_eq!(f["flash_mb"], json!(16));
    assert_eq!(f["serial"], json!("dc:da:0c:11:22:33"));
    assert_eq!(f["esp_app"], json!("helixscreen_esp32"));
    assert_eq!(f["helix_installed"], json!(true));
    assert_eq!(f["helix_version"], json!("v1.1.0-beta.9"));
}

#[test]
fn a_board_running_other_firmware_is_not_helixscreen() {
    let f = probe_facts(&s3(), &app_image("ktouch_stock", "2.1.0"));
    assert_eq!(f["esp_app"], json!("ktouch_stock"));
    assert!(!f.contains_key("helix_installed"));
    assert!(!f.contains_key("helix_version"));
    let erased = probe_facts(&s3(), &[0xff; 0x120]);
    assert!(!erased.contains_key("esp_app"));
    assert_eq!(erased["esp_chip"], json!("esp32s3"));
}

#[test]
fn linux_permission_help_gives_the_group_command_then_a_replug() {
    let help = permission_help("linux", "/dev/ttyUSB0", "uucp");
    assert!(help.contains("sudo usermod -aG uucp $USER"), "{help}");
    assert!(help.contains("log out"), "{help}");
    assert!(help.contains("unplug"), "{help}");
    let default = permission_help("linux", "/dev/ttyUSB0", "");
    assert!(
        default.contains("sudo usermod -aG dialout $USER"),
        "{default}"
    );
}

#[test]
fn other_systems_say_another_program_holds_the_port() {
    let help = permission_help("windows", "COM3", "");
    assert!(help.contains("COM3"), "{help}");
    assert!(help.contains("another program"), "{help}");
    assert!(!help.contains("usermod"), "{help}");
    assert!(permission_help("macos", "/dev/cu.usbserial-110", "").contains("unplug"));
}

#[test]
fn only_the_k_touch_usb_bridge_is_offered() {
    let found = candidates(&[
        port("/dev/ttyUSB0", 0x1a86, 0x7522),
        port("/dev/ttyUSB1", 0x1a86, 0x7523),
        port("/dev/ttyACM0", 0x1d50, 0x614e),
    ]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].host.addr, "/dev/ttyUSB0");
    assert_eq!(found[0].facts["usb"], json!("1a86:7522"));
    assert_eq!(usb_id(0x1a86, 0x7522), "1a86:7522");
}

#[test]
fn a_mac_bridge_listed_as_tty_and_cu_is_offered_once_by_its_cu_name() {
    let found = candidates(&[
        port("/dev/tty.usbserial-110", 0x1a86, 0x7522),
        port("/dev/cu.usbserial-110", 0x1a86, 0x7522),
    ]);
    let names: Vec<&str> = found.iter().map(|c| c.host.addr.as_str()).collect();
    assert_eq!(names, vec!["/dev/cu.usbserial-110"]);
}
