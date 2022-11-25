use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use goblin::pe::PE;
use tempfile::NamedTempFile;

pub fn lanzaboote_image(
    lanzaboote_stub: &Path,
    os_release: &Path,
    kernel_cmdline: &[String],
    kernel_path: &Path,
    initrd_path: &Path,
    esp: &Path,
) -> Result<PathBuf> {
    // objcopy copies files into the PE binary. That's why we have to write the contents
    // of some bootspec properties to disks
    let kernel_cmdline_file = write_to_tmp(kernel_cmdline.join(" "))?;
    let kernel_path_file = write_to_tmp(esp_relative_path_string(esp, kernel_path))?;
    let initrd_path_file = write_to_tmp(esp_relative_path_string(esp, initrd_path))?;

    let os_release_offs = stub_offset(lanzaboote_stub)?;
    let kernel_cmdline_offs = os_release_offs + file_size(&os_release)?;
    let initrd_path_offs = kernel_cmdline_offs + file_size(&kernel_cmdline_file)?;
    let kernel_path_offs = initrd_path_offs + file_size(&initrd_path_file)?;

    let sections = vec![
        s(".osrel", os_release, os_release_offs),
        s(".cmdline", kernel_cmdline_file, kernel_cmdline_offs),
        s(".initrdp", initrd_path_file, initrd_path_offs),
        s(".kernelp", kernel_path_file, kernel_path_offs),
    ];

    wrap_in_pe(&lanzaboote_stub, sections)
}

pub fn wrap_initrd(initrd_stub: &Path, initrd: &Path) -> Result<PathBuf> {
    let initrd_offs = stub_offset(initrd_stub)?;
    let sections = vec![s(".initrd", initrd, initrd_offs)];
    wrap_in_pe(initrd_stub, sections)
}

fn wrap_in_pe(stub: &Path, sections: Vec<Section>) -> Result<PathBuf> {
    let image = NamedTempFile::new().context("Failed to generate named temp file")?;

    let mut args: Vec<String> = sections.iter().flat_map(Section::to_objcopy).collect();
    let extra_args = vec![path_to_string(stub), path_to_string(&image)];
    args.extend(extra_args);

    let status = Command::new("objcopy")
        .args(&args)
        .status()
        .context("Failed to run objcopy command")?;
    if !status.success() {
        return Err(anyhow::anyhow!("Failed to wrap in pe with args `{:?}`", &args).into());
    }

    let (_, persistent_image) = image.keep().with_context(|| {
        format!(
            "Failed to persist image with stub: {} from temporary file",
            stub.display()
        )
    })?;
    Ok(persistent_image)
}

struct Section {
    name: &'static str,
    file_path: PathBuf,
    offset: u64,
}

impl Section {
    fn to_objcopy(&self) -> Vec<String> {
        vec![
            String::from("--add-section"),
            format!("{}={}", self.name, path_to_string(&self.file_path)),
            String::from("--change-section-vma"),
            format!("{}={:#x}", self.name, self.offset),
        ]
    }
}

fn s(name: &'static str, file_path: impl AsRef<Path>, offset: u64) -> Section {
    Section {
        name,
        file_path: file_path.as_ref().into(),
        offset,
    }
}

fn write_to_tmp(contents: impl AsRef<[u8]>) -> Result<PathBuf> {
    let mut tmpfile = NamedTempFile::new().context("Failed to create tempfile")?;
    tmpfile
        .write_all(contents.as_ref())
        .context("Failed to write to tempfile")?;
    Ok(tmpfile.keep()?.1)
}

fn esp_relative_path_string(esp: &Path, path: &Path) -> String {
    let relative_path = path
        .strip_prefix(esp)
        .expect("Failed to make path relative to esp")
        .to_owned();
    let relative_path_string = relative_path
        .into_os_string()
        .into_string()
        .expect("Failed to convert path '{}' to a relative string path")
        .replace("/", "\\");
    format!("\\{}", &relative_path_string)
}

fn stub_offset(binary: &Path) -> Result<u64> {
    let pe_binary = fs::read(binary).context("Failed to read PE binary file")?;
    let pe = PE::parse(&pe_binary).context("Failed to parse PE binary file")?;

    let image_base = image_base(&pe);

    // The Virtual Memory Addresss (VMA) is relative to the image base, aka the image base
    // needs to be added to the virtual address to get the actual (but still virtual address)
    Ok(u64::from(
        pe.sections
            .last()
            .and_then(|s| Some(s.virtual_size + s.virtual_address))
            .expect("Failed to calculate offset"),
    ) + image_base)
}

fn image_base(pe: &PE) -> u64 {
    pe.header
        .optional_header
        .expect("Failed to find optional header, you're fucked")
        .windows_fields
        .image_base
}

// All Linux file paths should be convertable to strings
fn path_to_string(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .to_owned()
        .into_os_string()
        .into_string()
        .expect(&format!(
            "Failed to convert path '{}' to a string",
            path.as_ref().display()
        ))
}

fn file_size(path: impl AsRef<Path>) -> Result<u64> {
    Ok(fs::File::open(path)?.metadata()?.size())
}
