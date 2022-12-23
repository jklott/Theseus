use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Path to the kernel image.
    #[arg(long)]
    kernel: PathBuf,
    /// Path to the modules directory.
    #[arg(long)]
    modules: PathBuf,
    /// Path at which the EFI image should be placed.
    #[arg(long)]
    efi_image: PathBuf,
    /// Path at which the EFI firmware should be placed.
    #[arg(long)]
    efi_firmware: PathBuf,
}

fn main() {
    let Args {
        kernel,
        modules,
        efi_image,
        efi_firmware,
    } = Args::parse();

    let mut bootloader = bootloader::UefiBoot::new(&kernel);

    for file in modules
        .read_dir()
        .expect("failed to open modules directory")
    {
        let file = file.expect("failed to read file");
        if file.file_type().expect("couldn't get file type").is_file() {
            bootloader.add_file(
                format!(
                    "modules/{}",
                    file.file_name()
                        .to_str()
                        .expect("couldn't convert path to str")
                )
                .into(),
                file.path(),
            );
        }
    }

    bootloader
        .create_disk_image(&efi_image)
        .expect("failed to create uefi disk image");

    std::fs::copy(ovmf_prebuilt::ovmf_pure_efi(), efi_firmware)
        .expect("couldn't copy efi firmware");
}