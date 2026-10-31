// Copyright © 2026, Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Parser for the flat VMDK text descriptor.
//!
//! A flat VMDK stores its layout in a small text descriptor: a header, a list
//! of `FLAT` extent lines, and a disk database (DDB). Only the `monolithicFlat`
//! and `twoGbMaxExtentFlat` create types are recognized.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::str::{FromStr, Lines};
use std::{io, iter};

use crate::AlignedFile;

// The maximum allowed VMDK descriptor length.
// Max matches QEMU's implementation.
const MAX_DESCRIPTOR_LEN: u64 = 1 << 20; // 1 MiB

/// Flat VMDK create types.
#[derive(Debug, Default)]
enum VMDKDiskType {
    #[default]
    CreateTypeUnsupported,
    MonolithicFlat,
    TwoGbMaxExtentFlat,
}

/// Extents Access Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtentAccess {
    /// "RW": readable and writable.
    ReadWrite,
    /// "RDONLY": readable only, writes must be rejected.
    ReadOnly,
    /// "NOACCESS": cannot be accessed, reads and writes must be rejected.
    NoAccess,
}

impl FromStr for ExtentAccess {
    type Err = io::Error;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        match token {
            "RW" => Ok(Self::ReadWrite),
            "RDONLY" => Ok(Self::ReadOnly),
            "NOACCESS" => Ok(Self::NoAccess),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported VMDK extent access mode '{other}'"),
            )),
        }
    }
}

/// Flat VMDK extent line fields.
/// Format of each extent line:
/// `<access> <sectors> <type> "<file>" [offset]`.
#[derive(Debug)]
pub(super) struct VmdkExtentHeader {
    pub access: ExtentAccess,
    pub size_in_sectors: u64,
    pub extent_type: String,
    pub filename: String,
    pub offset_in_sectors: u64,
}

/// Descriptor header fields.
#[derive(Debug, Default)]
struct VmdkDescriptorHeader {
    pub create_type: VMDKDiskType,
}

/// Ordered list of extents.
#[derive(Debug, Default)]
pub(super) struct VmdkDescriptorExtents {
    pub extents: Vec<VmdkExtentHeader>,
}

/// Parsed flat VMDK descriptor
#[derive(Debug, Default)]
pub(super) struct VmdkDescriptor {
    pub extents_list: VmdkDescriptorExtents,
}

impl VmdkDescriptor {
    pub(super) fn new(file: &File) -> io::Result<Self> {
        // Valid descriptor file must be much larger than 4 bytes.
        if file.metadata()?.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid VMDK descriptor file: file is empty or too small",
            ));
        }

        let content = read_descriptor(file)?;
        let mut lines = content.lines();
        let (header, last_line) = parse_header(&mut lines)?;
        if matches!(header.create_type, VMDKDiskType::CreateTypeUnsupported) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unsupported VMDK create type",
            ));
        }
        let extents_list = parse_extents(&mut lines, last_line)?;
        if extents_list.extents.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VMDK descriptor lists no extents",
            ));
        }
        for extent in &extents_list.extents {
            if extent.extent_type != "FLAT" {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Unsupported VMDK extent type '{}'", extent.extent_type),
                ));
            }
        }

        Ok(Self { extents_list })
    }
}

// Read the descriptor text into memory through an `AlignedFile` and return it
// as a `String`.
fn read_descriptor(file: &File) -> io::Result<String> {
    let aligned = AlignedFile::new(file.try_clone()?, true);

    let len = file.metadata()?.len();
    if len > MAX_DESCRIPTOR_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid VMDK descriptor: the file exceeds the maximum descriptor size",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    aligned.read_exact_at(&mut buf, 0)?;
    // QEMU can leave padding or stale bytes after the text terminator.
    if let Some(end) = buf.iter().position(|&byte| byte == 0) {
        buf.truncate(end);
    }
    // Decode descriptor text as UTF-8.
    String::from_utf8(buf).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "VMDK descriptor is not valid UTF-8",
        )
    })
}

fn parse_header<'a>(lines: &mut Lines<'a>) -> io::Result<(VmdkDescriptorHeader, &'a str)> {
    let mut header = VmdkDescriptorHeader::default();

    for line in lines.by_ref() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Ok((header, line));
        };
        if key.trim().contains(char::is_whitespace) {
            return Ok((header, line));
        }
        if key.trim() == "createType" {
            // Tools such as qemu-img quote the value, strip quotes for comparison.
            header.create_type = match value.trim().trim_matches('"') {
                "monolithicFlat" => VMDKDiskType::MonolithicFlat,
                "twoGbMaxExtentFlat" => VMDKDiskType::TwoGbMaxExtentFlat,
                _ => VMDKDiskType::CreateTypeUnsupported,
            };
        }
    }

    Ok((header, ""))
}

fn parse_extents<'a>(
    lines: &mut Lines<'a>,
    first_line: &'a str,
) -> io::Result<VmdkDescriptorExtents> {
    let mut extents = VmdkDescriptorExtents::default();

    for line in iter::once(first_line).chain(lines.by_ref()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("ddb.") {
            continue;
        }
        let invalid_extent =
            || io::Error::new(io::ErrorKind::InvalidData, "Malformed VMDK extent line");
        let (fields, file) = line.split_once('"').ok_or_else(invalid_extent)?;
        let (filename, offset) = file.split_once('"').ok_or_else(invalid_extent)?;
        let parts: Vec<&str> = fields.split_whitespace().collect();
        if parts.len() != 3 || filename.is_empty() {
            return Err(invalid_extent());
        }
        let size_in_sectors = parts[1].parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "VMDK extent size '{}' is not a valid sector count",
                    parts[1]
                ),
            )
        })?;
        let offset = offset.trim();
        let offset_in_sectors = if offset.is_empty() {
            0
        } else {
            offset.parse::<u64>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("VMDK extent offset '{offset}' is not a valid sector count"),
                )
            })?
        };
        extents.extents.push(VmdkExtentHeader {
            access: parts[0].parse::<ExtentAccess>()?,
            size_in_sectors,
            extent_type: parts[2].to_string(),
            filename: filename.to_string(),
            offset_in_sectors,
        });
    }

    Ok(extents)
}

/// Returns true for a supported flat VMDK: create type `monolithicFlat` or
/// `twoGbMaxExtentFlat` with only `FLAT` extents.
pub fn is_flat_vmdk(f: &mut File) -> io::Result<bool> {
    match VmdkDescriptor::new(f) {
        Ok(_) => Ok(true),
        Err(e) => match e.kind() {
            io::ErrorKind::InvalidData | io::ErrorKind::Unsupported => Ok(false),
            _ => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_hdr(input: &str) -> io::Result<(VmdkDescriptorHeader, String)> {
        let mut lines = input.lines();
        let (header, last) = parse_header(&mut lines)?;
        Ok((header, last.to_string()))
    }

    fn parse_body(last_comment: &str, body: &str) -> io::Result<VmdkDescriptorExtents> {
        let mut lines = body.lines();
        parse_extents(&mut lines, last_comment)
    }

    // Two-stage parse, as `VmdkDescriptor::new` chains it.
    fn parse_full(input: &str) -> io::Result<(VmdkDescriptorHeader, VmdkDescriptorExtents)> {
        let mut lines = input.lines();
        let (header, last) = parse_header(&mut lines)?;
        let extents = parse_extents(&mut lines, last)?;
        Ok((header, extents))
    }

    #[test]
    fn oversized_descriptor_is_rejected() {
        use std::io::Write;

        use vmm_sys_util::tempfile::TempFile;

        // A well-formed header followed by padding so the file exceeds the cap.
        let mut body = String::from("# Disk DescriptorFile\n");
        body.push_str(&"#\n".repeat((MAX_DESCRIPTOR_LEN as usize / 2) + 1));
        assert!(body.len() as u64 > MAX_DESCRIPTOR_LEN);

        let tmp = TempFile::new().unwrap();
        let mut file: &File = tmp.as_file();
        file.write_all(body.as_bytes()).unwrap();

        VmdkDescriptor::new(file).unwrap_err();
        assert!(!is_flat_vmdk(&mut tmp.into_file()).unwrap());
    }

    #[test]
    fn new_is_unaffected_by_shared_file_offset() {
        use std::io::{Read, Seek, SeekFrom, Write};

        use vmm_sys_util::tempfile::TempFile;

        // A minimal but complete monolithicFlat descriptor.
        let descriptor_text = "# Disk DescriptorFile\n\
                               version=1\n\
                               createType=monolithicFlat\n\
                               # Extent description\n\
                               RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                               # The Disk Data Base\n\
                               ddb.adapterType = \"ide\"\n";

        let tmp = TempFile::new().unwrap();
        let mut file: &File = tmp.as_file();
        file.write_all(descriptor_text.as_bytes()).unwrap();

        // Advance the shared OS file offset off zero, mimicking an earlier
        // image-type probe. `&File` is `Copy`, so this moves the same fd's
        // offset that `VmdkDescriptor::new` will see.
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut scratch = [0u8; 8];
        file.read_exact(&mut scratch).unwrap();
        assert_ne!(file.stream_position().unwrap(), 0);

        // `read_descriptor` reads positionally (anchored at offset 0), so the
        // advanced offset must not affect the parse.
        let descriptor = VmdkDescriptor::new(file).unwrap();
        assert_eq!(descriptor.extents_list.extents.len(), 1);
        assert_eq!(
            descriptor.extents_list.extents[0].filename,
            "disk-flat.vmdk"
        );
    }

    #[test]
    fn bytes_after_text_terminator_are_ignored() {
        use vmm_sys_util::tempfile::TempFile;

        for padding in [b"\0\0\0".as_slice(), b"\0\n", b"\0\xff\n"] {
            let mut text = b"CID=1\nparentCID=ffffffff\ncreateType=monolithicFlat\n\
                             NOACCESS 8 FLAT \"missing.vmdk\"\n"
                .to_vec();
            text.extend_from_slice(padding);
            let mut file = TempFile::new().unwrap().into_file();
            file.write_all_at(&text, 0).unwrap();
            assert!(is_flat_vmdk(&mut file).unwrap());
        }
    }

    #[test]
    fn single_flat_extent_with_ddb() {
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                          # The Disk Data Base\n\
                          ddb.adapterType = \"ide\"\n\
                          ddb.geometry.sectors = \"63\"\n";

        let extents = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 1);
        let e = &extents.extents[0];
        assert_eq!(e.access, ExtentAccess::ReadWrite);
        assert_eq!(e.size_in_sectors, 2_097_152);
        assert_eq!(e.extent_type, "FLAT");
        assert_eq!(e.filename, "disk-flat.vmdk");
    }

    #[test]
    fn multiple_extents_two_gb_max() {
        let body: &str = "RW 4192256 FLAT \"disk-s001.vmdk\"\n\
                          RW 4192256 FLAT \"disk-s002.vmdk\"\n\
                          RW 2097152 FLAT \"disk-s003.vmdk\"\n\
                          # The Disk Data Base\n\
                          ddb.adapterType = \"lsilogic\"\n";

        let extents = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 3);
        assert_eq!(extents.extents[0].filename, "disk-s001.vmdk");
        assert_eq!(extents.extents[2].filename, "disk-s003.vmdk");
        assert!(extents.extents.iter().all(|e| e.extent_type == "FLAT"));
    }

    #[test]
    fn extent_line_with_optional_offset_field() {
        // 5-field form: <access> <sectors> <type> <file> <offset>
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\" 0\n";

        let extents = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].filename, "disk-flat.vmdk");
    }

    #[test]
    fn extent_access_modes_are_preserved() {
        let body: &str = "RDONLY 2097152 FLAT \"ro.vmdk\"\n\
                          NOACCESS 1048576 FLAT \"noaccess.vmdk\"\n";

        let extents = parse_body("# Extent description", body).unwrap();

        assert_eq!(extents.extents.len(), 2);
        assert_eq!(extents.extents[0].access, ExtentAccess::ReadOnly);
        assert_eq!(extents.extents[1].access, ExtentAccess::NoAccess);
    }

    #[test]
    fn ignores_comments_before_extents() {
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\"\n";
        let extents = parse_body("# Another comment", body).unwrap();
        assert_eq!(extents.extents.len(), 1);
    }

    #[test]
    fn quoted_extent_filenames_are_preserved() {
        for filename in ["space disk.vmdk", "Hash#=Case.vmdk", " space # = disk "] {
            let body = format!("RW 128 FLAT \"{filename}\" 7\n");
            let extents = parse_body("", &body).unwrap();
            assert_eq!(extents.extents[0].filename, filename);
            assert_eq!(extents.extents[0].offset_in_sectors, 7);
        }
    }

    #[test]
    fn rejects_malformed_extent_line() {
        for body in [
            "RW 2097152 FLAT\n",
            "RW 128 FLAT bare.vmdk 0\n",
            "RW 128 FLAT \"unclosed.vmdk\n",
            "RW 128 FLAT \"\" 0\n",
            "RW 128 FLAT \"disk.vmdk\" 0 extra\n",
        ] {
            parse_body("", body).unwrap_err();
        }
    }

    #[test]
    fn rejects_unsupported_access_mode() {
        // Access must be one of RW / RDONLY / NOACCESS.
        let body: &str = "BOGUS 2097152 FLAT \"disk-flat.vmdk\"\n";
        parse_body("# Extent description", body).unwrap_err();
    }

    #[test]
    fn rejects_non_numeric_extent_size() {
        // A non-numeric sector count must be rejected, not coerced to 0.
        let body: &str = "RW notanumber FLAT \"disk-flat.vmdk\"\n";
        parse_body("# Extent description", body).unwrap_err();
    }

    #[test]
    fn rejects_non_numeric_extent_offset() {
        // A non-numeric trailing offset must be rejected too.
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\" xyz\n";
        parse_body("# Extent description", body).unwrap_err();
    }

    #[test]
    fn ignores_comments_inside_extents() {
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                          # Some other comment\n";
        let extents = parse_body("# Extent description", body).unwrap();
        assert_eq!(extents.extents.len(), 1);
    }

    #[test]
    fn skips_blank_line_inside_extent_section() {
        // qemu-img emits a blank line between the last extent and the
        // "# The Disk Data Base" marker, it must be skipped, not rejected.
        let body: &str = "RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                          \n\
                          # The Disk Data Base\n";
        let extents = parse_body("# Extent description", body).unwrap();
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].filename, "disk-flat.vmdk");
    }

    #[test]
    fn parses_header_fields() {
        let input: &str = "# Disk DescriptorFile\n\
                           version=1\n\
                           CID=fffffffe\n\
                           parentCID=ffffffff\n\
                           createType=monolithicFlat\n\
                           # Extent description\n";

        let (header, last) = parse_hdr(input).unwrap();

        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert!(last.is_empty());
    }

    #[test]
    fn parses_quoted_create_type() {
        let input: &str = "# Disk DescriptorFile\n\
                           version=1\n\
                           createType=\"twoGbMaxExtentFlat\"\n\
                           # Extent description\n";

        let (header, _last) = parse_hdr(input).unwrap();
        assert!(matches!(
            header.create_type,
            VMDKDiskType::TwoGbMaxExtentFlat
        ));
    }

    #[test]
    fn full_monolithic_flat_descriptor() {
        let input: &str = "# Disk DescriptorFile\n\
                           version=1\n\
                           createType=monolithicFlat\n\
                           # Extent description\n\
                           RW 2097152 FLAT \"disk-flat.vmdk\"\n\
                           # The Disk Data Base\n\
                           ddb.adapterType = \"ide\"\n";

        let (header, extents) = parse_full(input).unwrap();

        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].access, ExtentAccess::ReadWrite);
    }

    #[test]
    fn full_two_gb_max_extent_flat_descriptor() {
        let input: &str = "# Disk DescriptorFile\n\
                           version=1\n\
                           createType=twoGbMaxExtentFlat\n\
                           # Extent description\n\
                           RW 4192256 FLAT \"disk-s001.vmdk\"\n\
                           RW 4192256 FLAT \"disk-s002.vmdk\"\n\
                           # The Disk Data Base\n";

        let (header, extents) = parse_full(input).unwrap();

        assert!(matches!(
            header.create_type,
            VMDKDiskType::TwoGbMaxExtentFlat
        ));
        assert_eq!(extents.extents.len(), 2);
    }

    #[test]
    fn full_qemu_style_descriptor() {
        // Mirrors a real qemu-img monolithicFlat descriptor: quoted createType,
        // blank lines between sections, 5-field extent line with a trailing
        // offset, and the "#DDB" marker form.
        let input: &str = "# Disk DescriptorFile\n\
                           version=1\n\
                           CID=eb2295a4\n\
                           parentCID=ffffffff\n\
                           createType=\"monolithicFlat\"\n\
                           \n\
                           # Extent description\n\
                           RW 6291456 FLAT \"t-flat.vmdk\" 0\n\
                           \n\
                           # The Disk Data Base\n\
                           #DDB\n\
                           \n\
                           ddb.virtualHWVersion = \"4\"\n\
                           ddb.adapterType = \"ide\"\n";

        let (header, extents) = parse_full(input).unwrap();

        assert!(matches!(header.create_type, VMDKDiskType::MonolithicFlat));
        assert_eq!(extents.extents.len(), 1);
        assert_eq!(extents.extents[0].access, ExtentAccess::ReadWrite);
        assert_eq!(extents.extents[0].size_in_sectors, 6_291_456);
        assert_eq!(extents.extents[0].extent_type, "FLAT");
        assert_eq!(extents.extents[0].filename, "t-flat.vmdk");
    }
}
