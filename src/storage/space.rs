use super::*;

#[derive(Debug)]
pub(crate) struct InsufficientSpace {
    pub(crate) required: u64,
    pub(crate) available: u64,
}

impl std::fmt::Display for InsufficientSpace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "insufficient storage space: {} bytes required, {} bytes available",
            self.required, self.available
        )
    }
}

impl std::error::Error for InsufficientSpace {}

pub(crate) fn check(path: &Path, bytes: u64, reserve: u64) -> Result<()> {
    let available = fs2::available_space(path)?;
    let required = bytes
        .checked_add(reserve)
        .context("storage size overflow")?;
    if required > available {
        return Err(InsufficientSpace {
            required,
            available,
        }
        .into());
    }
    Ok(())
}

pub(super) fn archive_bytes<R: Read>(archive: &mut tar::Archive<R>, block: u64) -> Result<u64> {
    let mut bytes = 0_u64;
    for entry in archive.entries()? {
        let entry = entry?;
        let footprint = if let Some(header) = entry
            .header()
            .as_gnu()
            .filter(|header| !header.is_extended())
        {
            if entry.header().entry_type().is_gnu_sparse() {
                Footprint {
                    length: entry.size(),
                    data: entry.header().entry_size()?,
                    extents: header
                        .sparse
                        .iter()
                        .filter(|sparse| sparse.length().is_ok_and(|length| length > 0))
                        .count() as u64,
                }
            } else {
                Footprint::dense(entry.size())
            }
        } else {
            Footprint::dense(entry.size())
        };
        let needed = if entry.header().entry_type().is_dir() {
            block
        } else {
            footprint.required(block)?
        };
        bytes = bytes
            .checked_add(needed)
            .context("storage allocation overflow")?;
    }
    Ok(bytes)
}

pub(super) fn check_legacy(archive: &Path, stage: &Path, reserve: u64) -> Result<()> {
    let block = fs2::allocation_granularity(stage)?.max(1);
    let mut archive = tar::Archive::new(zstd::stream::Decoder::new(fs::File::open(archive)?)?);
    let required = archive_bytes(&mut archive, block)?;
    check(stage, required, reserve)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Footprint {
    pub(super) length: u64,
    pub(super) data: u64,
    pub(super) extents: u64,
}

impl Footprint {
    pub(super) fn of(path: &Path) -> Result<Self> {
        let length = fs::metadata(path)?.len();
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        {
            use rustix::fs::{SeekFrom, seek};
            let file = fs::File::open(path)?;
            let mut offset = 0;
            let mut data = 0_u64;
            let mut extents = 0_u64;
            while offset < length {
                let start = match seek(&file, SeekFrom::Data(offset)) {
                    Ok(start) => start,
                    Err(rustix::io::Errno::NXIO) => break,
                    Err(_) => return Ok(Self::dense(length)),
                };
                let end = match seek(&file, SeekFrom::Hole(start)) {
                    Ok(end) => end.min(length),
                    Err(_) => return Ok(Self::dense(length)),
                };
                ensure!(start < end && end <= length, "invalid sparse file extent");
                data = data
                    .checked_add(end - start)
                    .context("sparse size overflow")?;
                extents += 1;
                offset = end;
            }
            Ok(Self {
                length,
                data,
                extents,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
        Ok(Self::dense(length))
    }

    pub(super) fn dense(length: u64) -> Self {
        Self {
            length,
            data: length,
            extents: u64::from(length > 0),
        }
    }

    pub(super) fn required(&self, block: u64) -> Result<u64> {
        #[cfg(windows)]
        let (data, extents) = (self.length, u64::from(self.length > 0));
        #[cfg(not(windows))]
        let (data, extents) = (self.data, self.extents);
        // Each sparse data extent can straddle two allocation boundaries.
        extents
            .checked_mul(2)
            .and_then(|n| n.checked_mul(block))
            .and_then(|padding| data.checked_add(padding))
            .context("storage allocation overflow")
    }
}
