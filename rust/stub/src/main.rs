#![no_main]
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod linux_loader;
mod pe_loader;
mod pe_section;
mod uefi_helpers;
mod part_discovery;
mod measure;
mod unified_sections;
mod tpm;
mod cpio;
mod initrd;

use alloc::vec::Vec;
use log::{info, warn, debug};
use measure::measure_image;
use pe_loader::Image;
use pe_section::{pe_section, pe_section_as_string};
use sha2::{Digest, Sha256};
use tpm::tpm_available;
use uefi::{
    prelude::*,
    proto::{
        loaded_image::LoadedImage,
        media::file::{File, FileAttribute, FileMode},
    },
    CStr16, CString16, Result,
};
use uefi_helpers::SystemdLoaderFeatures;

use crate::{
    linux_loader::InitrdLoader,
    uefi_helpers::{booted_image_file, read_all, export_efi_variables, get_loader_features},
    initrd::CompanionInitrd
};

type Hash = sha2::digest::Output<Sha256>;

/// Print the startup logo on boot.
fn print_logo() {
    info!(
        "
  _                      _                 _
 | |                    | |               | |
 | | __ _ _ __  ______ _| |__   ___   ___ | |_ ___
 | |/ _` | '_ \\|_  / _` | '_ \\ / _ \\ / _ \\| __/ _ \\
 | | (_| | | | |/ / (_| | |_) | (_) | (_) | ||  __/
 |_|\\__,_|_| |_/___\\__,_|_.__/ \\___/ \\___/ \\__\\___|

"
    );
}

/// The configuration that is embedded at build time.
///
/// After lanzaboote is built, lzbt needs to embed configuration
/// into the binary. This struct represents that information.
struct EmbeddedConfiguration {
    /// The filename of the kernel to be booted. This filename is
    /// relative to the root of the volume that contains the
    /// lanzaboote binary.
    kernel_filename: CString16,

    /// The cryptographic hash of the kernel.
    kernel_hash: Hash,

    /// The filename of the initrd to be passed to the kernel. See
    /// `kernel_filename` for how to interpret these filenames.
    initrd_filename: CString16,

    /// The cryptographic hash of the initrd. This hash is computed
    /// over the whole PE binary, not only the embedded initrd.
    initrd_hash: Hash,

    /// The kernel command-line.
    cmdline: CString16,
}

/// Extract a string, stored as UTF-8, from a PE section.
fn extract_string(pe_data: &[u8], section: &str) -> Result<CString16> {
    let string = pe_section_as_string(pe_data, section).ok_or(Status::INVALID_PARAMETER)?;

    Ok(CString16::try_from(string.as_str()).map_err(|_| Status::INVALID_PARAMETER)?)
}

/// Extract a Blake3 hash from a PE section.
fn extract_hash(pe_data: &[u8], section: &str) -> Result<Hash> {
    let array: [u8; 32] = pe_section(pe_data, section)
        .ok_or(Status::INVALID_PARAMETER)?
        .try_into()
        .map_err(|_| Status::INVALID_PARAMETER)?;

    Ok(array.into())
}

impl EmbeddedConfiguration {
    fn new(file_data: &[u8]) -> Result<Self> {
        Ok(Self {
            kernel_filename: extract_string(file_data, ".kernelp")?,
            kernel_hash: extract_hash(file_data, ".kernelh")?,

            initrd_filename: extract_string(file_data, ".initrdp")?,
            initrd_hash: extract_hash(file_data, ".initrdh")?,

            cmdline: extract_string(file_data, ".cmdline")?,
        })
    }
}

/// Boot the Linux kernel without checking the PE signature.
///
/// We assume that the caller has made sure that the image is safe to
/// be loaded using other means.
fn boot_linux_unchecked(
    handle: Handle,
    system_table: SystemTable<Boot>,
    kernel_data: Vec<u8>,
    kernel_cmdline: &CStr16,
    initrd_data: Vec<u8>,
) -> uefi::Result<()> {
    let kernel =
        Image::load(system_table.boot_services(), &kernel_data).expect("Failed to load the kernel");

    let mut initrd_loader = InitrdLoader::new(system_table.boot_services(), handle, initrd_data)?;

    let status = unsafe { kernel.start(handle, &system_table, kernel_cmdline) };

    initrd_loader.uninstall(system_table.boot_services())?;
    status.into()
}

/// Boot the Linux kernel via the UEFI PE loader.
///
/// This should only succeed when UEFI Secure Boot is off (or
/// broken...), because the Lanzaboote tool does not sign the kernel.
///
/// In essence, we can use this routine to detect whether Secure Boot
/// is actually enabled.
fn boot_linux_uefi(
    handle: Handle,
    system_table: SystemTable<Boot>,
    kernel_data: Vec<u8>,
    kernel_cmdline: &CStr16,
    initrd_data: Vec<u8>,
) -> uefi::Result<()> {
    let kernel_handle = system_table.boot_services().load_image(
        handle,
        uefi::table::boot::LoadImageSource::FromBuffer {
            buffer: &kernel_data,
            file_path: None,
        },
    )?;

    let mut kernel_image = system_table
        .boot_services()
        .open_protocol_exclusive::<LoadedImage>(kernel_handle)?;

    unsafe {
        kernel_image.set_load_options(
            kernel_cmdline.as_ptr() as *const u8,
            // This unwrap is "safe" in the sense that any
            // command-line that doesn't fit 4G is surely broken.
            u32::try_from(kernel_cmdline.num_bytes()).unwrap(),
        );
    }

    let mut initrd_loader = InitrdLoader::new(system_table.boot_services(), handle, initrd_data)?;

    let status = system_table
        .boot_services()
        .start_image(kernel_handle)
        .status();

    initrd_loader.uninstall(system_table.boot_services())?;
    status.into()
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();

    print_logo();

    // SAFETY: We get a slice that represents our currently running
    // image and then parse the PE data structures from it. This is
    // safe, because we don't touch any data in the data sections that
    // might conceivably change while we look at the slice.
    let config: EmbeddedConfiguration = unsafe {
        EmbeddedConfiguration::new(
            booted_image_file(system_table.boot_services())
                .unwrap()
                .as_slice(),
        )
        .expect("Failed to extract configuration from binary. Did you run lzbt?")
    };

    let kernel_data;
    let initrd_data;

    {
        let mut file_system = system_table
            .boot_services()
            .get_image_file_system(handle)
            .expect("Failed to get file system handle");
        let mut root = file_system
            .open_volume()
            .expect("Failed to find ESP root directory");

        let mut kernel_file = root
            .open(
                &config.kernel_filename,
                FileMode::Read,
                FileAttribute::empty(),
            )
            .expect("Failed to open kernel file for reading")
            .into_regular_file()
            .expect("Kernel is not a regular file");

        kernel_data = read_all(&mut kernel_file).expect("Failed to read kernel file into memory");

        let mut initrd_file = root
            .open(
                &config.initrd_filename,
                FileMode::Read,
                FileAttribute::empty(),
            )
            .expect("Failed to open initrd for reading")
            .into_regular_file()
            .expect("Initrd is not a regular file");

        initrd_data = read_all(&mut initrd_file).expect("Failed to read kernel file into memory");
    }

    let is_kernel_hash_correct = Sha256::digest(&kernel_data) == config.kernel_hash;
    let is_initrd_hash_correct = Sha256::digest(&initrd_data) == config.initrd_hash;

    if !is_kernel_hash_correct {
        warn!("Hash mismatch for kernel!");
    }

    if !is_initrd_hash_correct {
        warn!("Hash mismatch for initrd!");
    }

    if tpm_available(system_table.boot_services()) {
        debug!("TPM available, will proceed to measurements.");
    }

    if let Ok(features) = get_loader_features(system_table.runtime_services()) {
        if features.contains(SystemdLoaderFeatures::RandomSeed) {
            // FIXME: process random seed then on the disk.
            debug!("Random seed is available, but lanzaboote does not support it yet.");
        }
    }

    unsafe {
        // Iterate over unified sections and measure them
        let _ = measure_image(&system_table, booted_image_file(
            system_table.boot_services()
        ).unwrap()).expect("Failed to measure the image");
    }

    export_efi_variables(&system_table)
        .expect("Failed to export stub EFI variables");

    let mut initrds = Vec::with_capacity(6);
    if let Ok(mut simple_filesystem) = system_table.boot_services().get_image_file_system(handle) {
        if let Ok(Some(credentials_initrd)) = cpio::pack_cpio(system_table.boot_services(),
            &mut simple_filesystem,
            None,
            cstr16!(".cred"),
            ".extra/credentials",
            0o500,
            0o400,
            measure::TPM_PCR_INDEX_KERNEL_PARAMETERS,
            "Credentials initrd") {
            initrds.push(CompanionInitrd::Credentials(credentials_initrd));
        }

        if let Ok(Some(global_credentials_initrd)) = cpio::pack_cpio(system_table.boot_services(),
            &mut simple_filesystem,
            Some(cstr16!("\\loader\\credentials")),
            cstr16!(".cred"),
            ".extra/global_credentials",
            0o500,
            0o400,
            measure::TPM_PCR_INDEX_KERNEL_PARAMETERS,
            "Global credentials initrd") {
            initrds.push(CompanionInitrd::GlobalCredentials(global_credentials_initrd));
        }

        if let Ok(Some(sysext_initrd)) = cpio::pack_cpio(system_table.boot_services(),
            &mut simple_filesystem,
            None,
            cstr16!(".raw"),
            ".extra/sysext",
            0o500,
            0o400,
            measure::TPM_PCR_INDEX_KERNEL_PARAMETERS,
            "System extension initrd") {
            initrds.push(CompanionInitrd::SystemExtension(sysext_initrd));
        }
    }

    // Let's export any StubPcr EFI variable we might need.
    let _ = initrd::export_pcr_efi_variables(&system_table.runtime_services(), initrds);

    if is_kernel_hash_correct && is_initrd_hash_correct {
        boot_linux_unchecked(
            handle,
            system_table,
            kernel_data,
            &config.cmdline,
            initrd_data,
        )
        .status()
    } else {
        // There is no good way to detect whether Secure Boot is
        // enabled. This is unfortunate, because we want to give the
        // user a way to recover from hash mismatches when Secure Boot
        // is off.
        //
        // So in case we get a hash mismatch, we will try to load the
        // Linux image using LoadImage. What happens then depends on
        // whether Secure Boot is enabled:
        //
        // **With Secure Boot**, the firmware will reject loading the
        // image with status::SECURITY_VIOLATION.
        //
        // **Without Secure Boot**, the firmware will just load the
        // Linux kernel.
        //
        // This is the behavior we want. A slight turd is that we
        // increase the attack surface here by exposing the unverfied
        // Linux image to the UEFI firmware. But in case the PE loader
        // of the firmware is broken, we have little hope of security
        // anyway.

        warn!("Trying to continue as non-Secure Boot. This will fail when Secure Boot is enabled.");

        boot_linux_uefi(
            handle,
            system_table,
            kernel_data,
            &config.cmdline,
            initrd_data,
        )
        .status()
    }
}
