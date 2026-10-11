//! FAT disk images for UEFI tests: an EFI system partition without a partition table
//! (UEFI firmware mounts such "superfloppy" disks too).

use std::io::{Cursor, Write};
use std::path::Path;

/// Writes a `size`-byte FAT image holding `files` in its root directory.
pub fn build(path: &Path, size: u64, files: &[(&str, &[u8])]) -> Result<(), String> {
    let bytes = image(size, files)?;
    std::fs::write(path, bytes).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// A `size`-byte FAT image holding `files` in its root directory.
pub fn image(size: u64, files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let error = |e: std::io::Error| format!("building a FAT image: {e}");
    let mut image = Cursor::new(vec![0u8; size as usize]);
    fatfs::format_volume(
        &mut image,
        fatfs::FormatVolumeOptions::new().volume_label(*b"VMMC ESP   "),
    )
    .map_err(error)?;
    {
        let fs = fatfs::FileSystem::new(&mut image, fatfs::FsOptions::new()).map_err(error)?;
        let root = fs.root_dir();
        for (name, data) in files {
            let mut file = root.create_file(name).map_err(error)?;
            file.truncate().map_err(error)?;
            file.write_all(data).map_err(error)?;
        }
        drop(root);
        fs.unmount().map_err(error)?;
    }
    Ok(image.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn files_read_back_from_the_image() {
        let dir = std::env::temp_dir().join(format!("vmmc-esp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("esp.raw");
        build(
            &path,
            8 << 20,
            &[("startup.nsh", b"echo hi\r\n"), ("big.bin", &[7; 100_000])],
        )
        .unwrap();
        let image = std::fs::read(&path).unwrap();
        let fs = fatfs::FileSystem::new(Cursor::new(image), fatfs::FsOptions::new()).unwrap();
        let mut text = String::new();
        fs.root_dir()
            .open_file("startup.nsh")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "echo hi\r\n");
        let mut big = Vec::new();
        fs.root_dir()
            .open_file("big.bin")
            .unwrap()
            .read_to_end(&mut big)
            .unwrap();
        assert_eq!(big, vec![7; 100_000]);
        drop(fs);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
