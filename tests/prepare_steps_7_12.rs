//! Integration: SSH key, cloud-init files, COW disk and ISO (steps 7–12).

use std::fs;

use virtum::cloud_init::{build_cloud_init_iso, write_meta_data, write_user_data};
use virtum::config;
use virtum::disk::create_cow_disk;
use virtum::images::{ensure_image, resolve_image};
use virtum::ssh::{ensure_ssh_keypair, read_public_key};
use virtum::test_env::isolated_virtum_state;

#[tokio::test]
async fn prepare_instance_assets_without_base_image_download() {
    let _state = isolated_virtum_state();
    let paths = config::ensure_layout().await.unwrap();

    let keys = ensure_ssh_keypair(&paths.keys).unwrap();
    let pubkey = read_public_key(&keys.public_key).unwrap();

    let vm_id = "vm-test01";
    let instance_dir = paths.instances.join(vm_id);
    fs::create_dir_all(&instance_dir).unwrap();

    write_user_data(&instance_dir, "virtum", &pubkey, false).unwrap();
    write_meta_data(&instance_dir, vm_id, vm_id).unwrap();

    if std::process::Command::new("cloud-localds")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        build_cloud_init_iso(&instance_dir).unwrap();
        assert!(instance_dir.join("cloud-init.iso").is_file());
    }

    if std::process::Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let base = paths.images.join("debian-12.qcow2");
        virtum::command::run_command(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                virtum::command::path_arg(&base).unwrap(),
                "32M",
            ],
        )
        .unwrap();
        let disk = instance_dir.join("disk.qcow2");
        create_cow_disk(&base, &disk, None).unwrap();
        assert!(disk.is_file());
    }
}

#[tokio::test]
#[ignore = "requires network and ~400MB download"]
async fn download_debian_12_image() {
    let _state = isolated_virtum_state();
    let paths = config::ensure_layout().await.unwrap();
    let image = resolve_image("debian-12", &paths.images).unwrap();
    ensure_image(&image).await.unwrap();
    assert!(image.local_path.is_file());
    ensure_image(&image).await.unwrap();
}
