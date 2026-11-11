//! The plugin-owned directory the host writes PDFs into.
//!
//! `prepare_documents` is also listed as an MCP tool, so it accepts a bare
//! file name of a fixed shape and nothing else, resolves it only here, opens
//! it without following links, and checks the open descriptor before reading.

use crate::error::DocError;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PREFIX: &str = "mesh-doc-";
const SUFFIX: &str = ".pdf";

pub struct Staging {
    dir: PathBuf,
    owner: u32,
}

impl Staging {
    pub fn create(dir: PathBuf) -> std::io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let metadata = std::fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() {
            return Err(std::io::Error::other(format!(
                "staging path {} is not a directory",
                dir.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            dir,
            owner: metadata.uid(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read one staged PDF, at most `max_bytes` long.
    pub fn read(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>, DocError> {
        if !valid_name(name) {
            return Err(DocError::new(
                400,
                "invalid_staged_name",
                "staged must be a file name of the form mesh-doc-<32 hex>.pdf",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.dir.join(name))
            .map_err(|error| {
                DocError::new(400, "invalid_staged_name", format!("staged file: {error}"))
            })?;
        self.check_open_file(&file, max_bytes)?;
        let mut bytes = Vec::new();
        file.take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(DocError::internal)?;
        if bytes.len() as u64 > max_bytes {
            return Err(DocError::too_large(
                "PDF attachments exceed max_total_pdf_mb",
            ));
        }
        if !bytes.starts_with(b"%PDF-") {
            return Err(DocError::new(422, "invalid_pdf", "attachment is not a PDF"));
        }
        Ok(bytes)
    }

    fn check_open_file(&self, file: &File, max_bytes: u64) -> Result<(), DocError> {
        let metadata = file.metadata().map_err(DocError::internal)?;
        if !metadata.is_file() || metadata.uid() != self.owner {
            return Err(DocError::new(
                400,
                "invalid_staged_name",
                "staged file is not a regular file owned by this plugin's user",
            ));
        }
        if metadata.len() > max_bytes {
            return Err(DocError::too_large(
                "PDF attachments exceed max_total_pdf_mb",
            ));
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    const NAME: &str = "mesh-doc-0123456789abcdef0123456789abcdef.pdf";

    fn staging() -> (TempDir, Staging) {
        let dir = TempDir::new();
        let staging = Staging::create(dir.path().join("staging")).unwrap();
        (dir, staging)
    }

    #[test]
    fn only_the_fixed_name_shape_is_accepted() {
        assert!(valid_name(NAME));
        for bad in [
            "../mesh-doc-0123456789abcdef0123456789abcdef.pdf",
            "mesh-doc-0123456789ABCDEF0123456789abcdef.pdf",
            "mesh-doc-0123.pdf",
            "/tmp/mesh-doc-0123456789abcdef0123456789abcdef.pdf",
            "mesh-doc-0123456789abcdef0123456789abcdef.pdf/..",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_directory_is_private_and_reads_check_the_file() {
        let (_dir, staging) = staging();
        let mode = std::fs::metadata(staging.dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        std::fs::write(staging.dir().join(NAME), b"%PDF-1.7 body").unwrap();
        assert_eq!(staging.read(NAME, 1024).unwrap(), b"%PDF-1.7 body");
        assert_eq!(staging.read(NAME, 4).unwrap_err().status, 413);

        std::fs::write(staging.dir().join(NAME), b"not a pdf").unwrap();
        assert_eq!(staging.read(NAME, 1024).unwrap_err().code, "invalid_pdf");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let (dir, staging) = staging();
        let target = dir.path().join("secret.pdf");
        std::fs::write(&target, b"%PDF-1.7 secret").unwrap();
        std::os::unix::fs::symlink(&target, staging.dir().join(NAME)).unwrap();
        assert_eq!(
            staging.read(NAME, 1024).unwrap_err().code,
            "invalid_staged_name"
        );
    }
}
