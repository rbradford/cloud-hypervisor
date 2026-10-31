// Copyright © 2026, Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Flat VMDK block backend.
//!
//! Supports the `monolithicFlat` and `twoGbMaxExtentFlat` create types with
//! synchronous, extent-aware I/O.

mod descriptor;
mod engine_sync;
mod flat;

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub use descriptor::is_flat_vmdk;

use self::engine_sync::FlatVmdkSync;
use self::flat::FlatVmdk;
use crate::async_io::{AsyncIo, BorrowedDiskFd, DiskFileError};
use crate::error::{BlockError, BlockErrorKind, BlockResult, ErrorOp};
use crate::{DiskTopology, disk_file};

#[derive(Debug)]
pub struct VmdkDisk {
    inner: FlatVmdk,
}

impl VmdkDisk {
    /// Builds a Flat VMDK disk backend.
    pub fn new(file: File, path: &Path, readonly: bool, direct: bool) -> Result<Self, BlockError> {
        let inner = FlatVmdk::new(file, path, readonly, direct).map_err(|e| {
            let kind = match e.kind() {
                io::ErrorKind::InvalidData => BlockErrorKind::InvalidFormat,
                io::ErrorKind::Unsupported => BlockErrorKind::UnsupportedFeature,
                _ => BlockErrorKind::Io,
            };
            BlockError::new(kind, e).with_op(ErrorOp::Open)
        })?;
        Ok(VmdkDisk { inner })
    }
}

impl disk_file::DiskSize for VmdkDisk {
    fn logical_size(&self) -> BlockResult<u64> {
        Ok(self.inner.virtual_block_size())
    }
}

impl disk_file::PhysicalSize for VmdkDisk {
    fn physical_size(&self) -> BlockResult<u64> {
        Ok(self.inner.physical_block_size())
    }
}

// Expose the descriptor file's fd for advisory image locking.
impl disk_file::DiskFd for VmdkDisk {
    fn fd(&self) -> BorrowedDiskFd<'_> {
        BorrowedDiskFd::new(self.inner.as_raw_fd())
    }
}

impl disk_file::Geometry for VmdkDisk {
    fn topology(&self) -> DiskTopology {
        self.inner.topology()
    }
}

impl disk_file::SparseCapable for VmdkDisk {}

// The CID is persisted before the first data write.
impl disk_file::MetadataSync for VmdkDisk {}

impl disk_file::Resizable for VmdkDisk {
    fn resize(&mut self, _size: u64) -> BlockResult<()> {
        Err(BlockError::new(
            BlockErrorKind::UnsupportedFeature,
            DiskFileError::ResizeError(io::Error::other("resize not supported for flat VMDK")),
        )
        .with_op(ErrorOp::Resize))
    }
}

impl disk_file::DiskFile for VmdkDisk {}

impl disk_file::AsyncDiskFile for VmdkDisk {
    fn try_clone(&self) -> BlockResult<Box<dyn disk_file::AsyncDiskFile>> {
        Ok(Box::new(VmdkDisk {
            inner: self.inner.clone(),
        }))
    }

    fn create_async_io(&self, ring_depth: u32) -> BlockResult<Box<dyn AsyncIo>> {
        // VMDK provides a synchronous, extent-aware worker, so the io_uring ring
        // depth is unused here.
        let _ = ring_depth;

        Ok(Box::new(FlatVmdkSync::new(self.inner.clone())))
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    use vmm_sys_util::tempdir::TempDir;

    use super::*;
    use crate::async_io::OwnedIoBuffer;
    use crate::disk_file::{AsyncDiskFile, DiskFd, DiskSize, PhysicalSize, Resizable};
    use crate::formats::vmdk::descriptor::ExtentAccess;

    const SECTOR: u64 = 512;

    // Builds a flat VMDK in `dir`: a descriptor plus one backing data file per
    // extent. When `allocate` is true the extent files are filled with real
    // blocks (fixed / pre-allocated layout used in practice), otherwise they
    // are created sparse via `set_len` (declared length but no allocated
    // blocks). `extents` entries are (filename, access, sectors). Returns the
    // descriptor path.
    fn build_flat_vmdk(
        dir: &Path,
        create_type: &str,
        extents: &[(&str, &str, u64)],
        allocate: bool,
    ) -> PathBuf {
        let mut desc = String::from("# Disk DescriptorFile\n");
        desc.push_str("version=1\n");
        desc.push_str("CID=fffffffe\n");
        desc.push_str("parentCID=ffffffff\n");
        desc.push_str(&format!("createType={create_type}\n"));
        desc.push_str("# Extent description\n");

        for (filename, access, sectors) in extents {
            let mut data = File::create(dir.join(filename)).unwrap();
            if allocate {
                data.write_all(&vec![0u8; (sectors * SECTOR) as usize])
                    .unwrap();
            } else {
                data.set_len(sectors * SECTOR).unwrap();
            }
            data.sync_all().unwrap();
            desc.push_str(&format!("{access} {sectors} FLAT \"{filename}\"\n"));
        }

        desc.push_str("# The Disk Data Base\n");
        desc.push_str("ddb.adapterType = \"ide\"\n");

        let desc_path = dir.join("disk.vmdk");
        let mut df = File::create(&desc_path).unwrap();
        df.write_all(desc.as_bytes()).unwrap();
        df.sync_all().unwrap();
        desc_path
    }

    // Sparse extents.
    fn write_flat_vmdk(dir: &Path, create_type: &str, extents: &[(&str, &str, u64)]) -> PathBuf {
        build_flat_vmdk(dir, create_type, extents, false)
    }

    // Fully pre-allocated extents.
    fn write_flat_vmdk_allocated(
        dir: &Path,
        create_type: &str,
        extents: &[(&str, &str, u64)],
    ) -> PathBuf {
        build_flat_vmdk(dir, create_type, extents, true)
    }

    fn open_descriptor(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    // Writes a descriptor referencing `extent_lines` verbatim (no backing data
    // files are created). Used to exercise `FlatVmdk::new`'s per-extent
    // validation, whose zero-size/overflow checks all run before an extent file
    // would be opened.
    fn write_descriptor(dir: &Path, create_type: &str, extent_lines: &[&str]) -> PathBuf {
        let mut desc = String::from("# Disk DescriptorFile\n");
        desc.push_str("version=1\n");
        desc.push_str("CID=fffffffe\n");
        desc.push_str("parentCID=ffffffff\n");
        desc.push_str(&format!("createType={create_type}\n"));
        desc.push_str("# Extent description\n");
        for line in extent_lines {
            desc.push_str(line);
            desc.push('\n');
        }
        desc.push_str("# The Disk Data Base\n");
        desc.push_str("ddb.adapterType = \"ide\"\n");

        let desc_path = dir.join("disk.vmdk");
        let mut df = File::create(&desc_path).unwrap();
        df.write_all(desc.as_bytes()).unwrap();
        df.sync_all().unwrap();
        desc_path
    }

    #[test]
    fn logical_and_physical_size_single_extent() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 2048)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        assert_eq!(disk.logical_size().unwrap(), 2048 * SECTOR);
        // The extent is created sparse (`set_len`), so no blocks are allocated
        // and the `st_blocks`-based physical size is 0.
        assert_eq!(disk.physical_size().unwrap(), 0);
    }

    #[test]
    fn descriptor_directory_need_not_be_utf8() {
        use std::ffi::OsStr;
        use std::fs;
        use std::os::unix::ffi::OsStrExt;

        let dir = TempDir::new_with_prefix("/tmp/vmdk-path-test").unwrap();
        let parent = dir.as_path().join(OsStr::from_bytes(b"disk-\xff"));
        fs::create_dir(&parent).unwrap();
        let path = write_flat_vmdk(&parent, "monolithicFlat", &[("data.vmdk", "RW", 8)]);
        let disk = VmdkDisk::new(open_descriptor(&path), &path, true, false).unwrap();
        assert_eq!(disk.logical_size().unwrap(), 8 * SECTOR);
    }

    #[test]
    fn logical_size_sums_multiple_extents() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "twoGbMaxExtentFlat",
            &[("s001.vmdk", "RW", 2048), ("s002.vmdk", "RW", 1024)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        assert_eq!(disk.logical_size().unwrap(), (2048 + 1024) * SECTOR);
        // Sparse extents: no blocks are allocated, so physical size is 0.
        assert_eq!(disk.physical_size().unwrap(), 0);
    }

    #[test]
    fn physical_size_matches_fully_allocated_extents() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk_allocated(
            dir.as_path(),
            "twoGbMaxExtentFlat",
            &[("s001.vmdk", "RW", 2048), ("s002.vmdk", "RW", 1024)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        // Fully pre-allocated extents: host allocation (st_blocks) equals the
        // declared logical size.
        assert_eq!(disk.logical_size().unwrap(), (2048 + 1024) * SECTOR);
        assert_eq!(disk.physical_size().unwrap(), (2048 + 1024) * SECTOR);
    }

    #[test]
    fn fd_exposes_descriptor_file() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 64)],
        );
        let file = open_descriptor(&path);
        let expected = file.as_raw_fd();

        let disk = VmdkDisk::new(file, &path, false, false).unwrap();

        assert_eq!(disk.fd().as_raw_fd(), expected);
    }

    // An out-of-range request must be rejected, not abort while formatting
    // the error message describing it.
    #[test]
    fn out_of_range_request_reports_error_without_overflow() {
        let dir = TempDir::new().unwrap();
        let path = build_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("e-flat.vmdk", "RW", 8)],
            true,
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let disk = VmdkDisk::new(file, &path, false, false).unwrap();
        let mut io = disk.create_async_io(1).unwrap();

        let buffer = OwnedIoBuffer::from_vec(vec![0u8; 512]);
        let result = io.read_to_vec(u64::MAX as libc::off_t, buffer, 0);

        assert!(result.is_err());
    }

    #[test]
    fn readonly_open_downgrades_rw_extent() {
        // The caller asked for a read-only disk, so the "RW" extent must be
        // opened without write access even though the descriptor allows it.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-ro-test").unwrap();
        let path = write_flat_vmdk_allocated(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 8)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, true, false).unwrap();

        let extents = disk.inner.extents();
        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].access, ExtentAccess::ReadOnly);
        assert!(
            !extents[0].file.as_ref().unwrap().is_writable(),
            "extent fd must not carry write access for a readonly open"
        );

        // And a write through the disk is refused rather than silently
        // reaching the host file.
        let mut io = disk.create_async_io(1).unwrap();
        let buffer = OwnedIoBuffer::from_vec(vec![0xAAu8; 512]);
        assert!(io.write_from_vec(0, buffer, 0).is_err());
    }

    #[test]
    fn writable_open_keeps_rw_extent_writable() {
        // Without a readonly request the descriptor's "RW" is honoured.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-rw-test").unwrap();
        let path = write_flat_vmdk_allocated(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 8)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        let extents = disk.inner.extents();
        assert_eq!(extents[0].access, ExtentAccess::ReadWrite);
        assert!(extents[0].file.as_ref().unwrap().is_writable());
    }

    #[test]
    fn cid_changes_once_before_data_writes() {
        use std::fs;

        let dir = TempDir::new_with_prefix("/tmp/vmdk-cid-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "twoGbMaxExtentFlat",
            &[
                ("Data #=flat.vmdk", "RW", 8),
                ("readonly.vmdk", "RDONLY", 8),
            ],
        );
        let original = fs::read_to_string(&path)
            .unwrap()
            .replace("CID=fffffffe", "  cId = \"1\" ")
            .replace('\n', "\r\n");
        fs::write(&path, &original).unwrap();
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();
        let mut first = disk.create_async_io(1).unwrap();
        let mut second = disk.try_clone().unwrap().create_async_io(1).unwrap();
        drop(disk);

        first
            .read_to_vec(0, OwnedIoBuffer::from_vec(vec![0; 512]), 0)
            .unwrap();
        assert_eq!(first.next_completed_request().unwrap().result, 512);
        first
            .write_from_vec(0, OwnedIoBuffer::from_vec(Vec::new()), 0)
            .unwrap();
        assert_eq!(first.next_completed_request().unwrap().result, 0);
        first.fsync(None).unwrap();
        for (offset, length) in [(-1, 512), (8192, 512), (3584, 1024), (4096, 512)] {
            assert!(
                first
                    .write_from_vec(offset, OwnedIoBuffer::from_vec(vec![1; length]), 0)
                    .is_err()
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read(dir.as_path().join("Data #=flat.vmdk")).unwrap(),
            vec![0; 4096]
        );

        first
            .write_from_vec(0, OwnedIoBuffer::from_vec(vec![1; 512]), 0)
            .unwrap();
        assert_eq!(first.next_completed_request().unwrap().result, 512);
        let updated = fs::read_to_string(&path).unwrap();
        let cid = updated
            .lines()
            .find_map(|line| line.strip_prefix("CID="))
            .unwrap();
        assert_ne!(u32::from_str_radix(cid, 16).unwrap(), 1);
        assert_eq!(
            updated,
            original.replace("  cId = \"1\" ", &format!("CID={cid}"))
        );
        second
            .write_from_vec(512, OwnedIoBuffer::from_vec(vec![2; 512]), 0)
            .unwrap();
        assert_eq!(second.next_completed_request().unwrap().result, 512);
        assert_eq!(fs::read_to_string(&path).unwrap(), updated);
        let data = fs::read(dir.as_path().join("Data #=flat.vmdk")).unwrap();
        assert_eq!(data[..512], [1; 512]);
        assert_eq!(data[512..1024], [2; 512]);
    }

    #[test]
    fn cid_write_failure_prevents_data_writes() {
        use std::fs;

        let dir = TempDir::new_with_prefix("/tmp/vmdk-cid-error-test").unwrap();
        let path = write_flat_vmdk(dir.as_path(), "monolithicFlat", &[("data.vmdk", "RW", 8)]);
        let original = fs::read(&path).unwrap();
        let disk = VmdkDisk::new(File::open(&path).unwrap(), &path, false, false).unwrap();
        let pending = disk.inner.write_lock.lock().unwrap().clone();
        let mut io = disk.create_async_io(1).unwrap();
        for _ in 0..2 {
            assert!(
                io.write_from_vec(0, OwnedIoBuffer::from_vec(vec![1; 512]), 0)
                    .is_err()
            );
            assert!(io.next_completed_request().is_none());
            assert_eq!(*disk.inner.write_lock.lock().unwrap(), pending);
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read(dir.as_path().join("data.vmdk")).unwrap(),
            vec![0; 4096]
        );
    }

    #[test]
    fn descriptor_rdonly_extent_stays_readonly_for_writable_open() {
        // A descriptor-declared RDONLY extent stays read-only even when the
        // caller did not ask for a readonly disk.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-rdonly-test").unwrap();
        let path = write_flat_vmdk_allocated(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RDONLY", 8)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        let extents = disk.inner.extents();
        assert_eq!(extents[0].access, ExtentAccess::ReadOnly);
        assert!(!extents[0].file.as_ref().unwrap().is_writable());

        let mut io = disk.create_async_io(1).unwrap();
        let buffer = OwnedIoBuffer::from_vec(vec![0xAAu8; 512]);
        assert!(io.write_from_vec(0, buffer, 0).is_err());
    }

    #[test]
    fn resize_is_unsupported() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 64)],
        );
        let mut disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        let err = disk.resize(4096).unwrap_err();
        assert_eq!(err.kind(), BlockErrorKind::UnsupportedFeature);
    }

    #[test]
    fn try_clone_preserves_size() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 2048)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        let cloned = disk.try_clone().unwrap();
        assert_eq!(cloned.logical_size().unwrap(), disk.logical_size().unwrap());
    }

    #[test]
    fn create_async_io_builds_worker() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "monolithicFlat",
            &[("disk-flat.vmdk", "RW", 2048)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        // Ring depth is ignored by the synchronous VMDK worker.
        disk.create_async_io(0).unwrap();
    }

    #[test]
    fn create_async_io_supports_multi_extent() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "twoGbMaxExtentFlat",
            &[("s001.vmdk", "RW", 2048), ("s002.vmdk", "RW", 2048)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();

        disk.create_async_io(32).unwrap();
    }

    #[test]
    fn comments_do_not_define_descriptor_sections() {
        use std::os::unix::fs::FileExt;

        use vmm_sys_util::tempfile::TempFile;

        use crate::{ImageType, validate_image_type};

        let text = " \n  # A descriptor comment\n\
                    version = 1\nCID = fffffffe\nparentCID = ffffffff\n\
                    createType = \"monolithicFlat\"\n\
                    # Another comment\n\
                    NOACCESS 8 FLAT \"missing.vmdk\" 0  \n\
                    # Disk properties\n\
                    ddb.adapterType = \"ide\"\n";
        let file = TempFile::new().unwrap();
        file.as_file().write_all_at(text.as_bytes(), 0).unwrap();
        let mut descriptor = file.as_file().try_clone().unwrap();
        assert!(validate_image_type(&mut descriptor, ImageType::FlatVmdk).unwrap());
        let disk = VmdkDisk::new(descriptor, file.as_path(), false, false).unwrap();
        assert_eq!(disk.logical_size().unwrap(), 8 * SECTOR);
    }

    #[test]
    fn unsupported_extents_are_rejected_before_open() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-format-test").unwrap();
        for (create_type, extent_type) in [
            ("monolithicSparse", "FLAT"),
            ("monolithicFlat", "SPARSE"),
            ("twoGbMaxExtentFlat", "VMFS"),
        ] {
            let line = format!("NOACCESS 8 {extent_type} \"missing.vmdk\"");
            let path = write_descriptor(dir.as_path(), create_type, &[&line]);
            let mut file = open_descriptor(&path);
            assert!(!is_flat_vmdk(&mut file).unwrap());
            let error = VmdkDisk::new(file, &path, false, false).unwrap_err();
            assert_eq!(error.kind(), BlockErrorKind::UnsupportedFeature);
        }

        let path = write_descriptor(dir.as_path(), "monolithicFlat", &[]);
        let mut file = open_descriptor(&path);
        assert!(!is_flat_vmdk(&mut file).unwrap());
        let error = VmdkDisk::new(file, &path, false, false).unwrap_err();
        assert_eq!(error.kind(), BlockErrorKind::InvalidFormat);
    }

    #[test]
    fn extent_ranges_must_fit_the_backing_file() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-size-test").unwrap();
        let data = File::create(dir.as_path().join("data.vmdk")).unwrap();
        for access in ["RW", "RDONLY"] {
            for offset in [0, 7] {
                let line = format!("{access} 8 FLAT \"data.vmdk\" {offset}");
                let path = write_descriptor(dir.as_path(), "monolithicFlat", &[&line]);
                let end = (offset + 8) * SECTOR;
                data.set_len(end - 1).unwrap();
                let error = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap_err();
                assert_eq!(error.kind(), BlockErrorKind::InvalidFormat);
                data.set_len(end).unwrap();
                VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();
            }
        }
    }

    #[test]
    fn short_reads_do_not_complete_successfully() {
        use crate::async_io::AsyncIoError;

        let dir = TempDir::new_with_prefix("/tmp/vmdk-short-test").unwrap();
        let path = write_flat_vmdk(
            dir.as_path(),
            "twoGbMaxExtentFlat",
            &[("a.vmdk", "RW", 8), ("b.vmdk", "RW", 8)],
        );
        let disk = VmdkDisk::new(open_descriptor(&path), &path, false, false).unwrap();
        File::create(dir.as_path().join("b.vmdk")).unwrap();
        let mut io = disk.create_async_io(1).unwrap();
        for (offset, length) in [(4096, 512), (3584, 1024)] {
            let buffer = OwnedIoBuffer::from_vec(vec![0; length]);
            let error = io.read_to_vec(offset, buffer, 0).unwrap_err();
            assert!(
                matches!(error, AsyncIoError::ReadVectored(e) if e.kind() == io::ErrorKind::UnexpectedEof)
            );
            assert!(io.next_completed_request().is_none());
        }
    }

    #[test]
    fn new_rejects_zero_sector_extent() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-zero-test").unwrap();
        let path = write_descriptor(
            dir.as_path(),
            "monolithicFlat",
            &["RW 0 FLAT \"disk-flat.vmdk\""],
        );

        let err = FlatVmdk::new(open_descriptor(&path), &path, false, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn new_rejects_extent_size_overflow() {
        // size_in_sectors * 512 must fit in a u64.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-ovf-size-test").unwrap();
        let line = format!("RW {} FLAT \"disk-flat.vmdk\"", u64::MAX);
        let path = write_descriptor(dir.as_path(), "monolithicFlat", &[line.as_str()]);

        let err = FlatVmdk::new(open_descriptor(&path), &path, false, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn new_rejects_extent_file_offset_overflow() {
        // The offset * 512 must fit in a u64.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-ovf-offset-test").unwrap();
        let line = format!("RW 1 FLAT \"disk-flat.vmdk\" {}", u64::MAX);
        let path = write_descriptor(dir.as_path(), "monolithicFlat", &[line.as_str()]);

        let err = FlatVmdk::new(open_descriptor(&path), &path, false, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn new_rejects_extent_file_range_overflow() {
        // Each of offset*512 and size*512 fits in a u64, but their sum (the last
        // byte the extent addresses in its backing file) overflows.
        let dir = TempDir::new_with_prefix("/tmp/vmdk-ovf-range-test").unwrap();
        // offset_in_sectors = floor(u64::MAX / 512) => offset bytes = u64::MAX -
        // 511, size 1 sector (512 bytes) pushes the end one byte past u64::MAX.
        let offset = u64::MAX / 512;
        let line = format!("RW 1 FLAT \"disk-flat.vmdk\" {offset}");
        let path = write_descriptor(dir.as_path(), "monolithicFlat", &[line.as_str()]);

        let err = FlatVmdk::new(open_descriptor(&path), &path, false, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn monolithic_flat_rejects_multiple_extents() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-monolithic-test").unwrap();
        let line = "NOACCESS 8 FLAT \"missing.vmdk\"";
        let path = write_descriptor(dir.as_path(), "monolithicFlat", &[line, line]);
        let mut file = open_descriptor(&path);
        assert!(!is_flat_vmdk(&mut file).unwrap());
        assert_eq!(
            FlatVmdk::new(file, &path, false, false).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn split_flat_extents_are_limited_to_two_gib() {
        let dir = TempDir::new_with_prefix("/tmp/vmdk-split-size-test").unwrap();
        for sectors in [1 << 22, (1 << 22) + 1] {
            let line = format!("NOACCESS {sectors} FLAT \"s001.vmdk\"");
            let path = write_descriptor(
                dir.as_path(),
                "twoGbMaxExtentFlat",
                &[&line, "NOACCESS 1 FLAT \"s002.vmdk\""],
            );
            let mut file = open_descriptor(&path);
            let valid = sectors == 1 << 22;
            assert_eq!(is_flat_vmdk(&mut file).unwrap(), valid);
            let result = FlatVmdk::new(file, &path, false, false);
            assert_eq!(result.is_ok(), valid);
            if let Err(error) = result {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            }
        }
    }
}
