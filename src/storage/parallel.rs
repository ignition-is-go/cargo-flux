//! Indexed, independently compressed tar shards. Legacy snapshots remain readable.
use super::*;
use std::io::{BufReader, BufWriter, Seek, SeekFrom};
use std::sync::Mutex;

pub(super) const MAGIC: &[u8; 8] = b"FLUXSH02";
const MAX_HEADER: u64 = 64 * 1024 * 1024;
const MAX_SHARDS: usize = 8;

#[derive(Serialize, Deserialize)]
struct Index {
    manifest: Manifest,
    directories: BTreeMap<PathBuf, u32>,
    shards: Vec<Shard>,
}

#[derive(Serialize, Deserialize)]
struct Shard {
    extents: Vec<(u64, u64)>,
    files: BTreeSet<PathBuf>,
}

fn directories(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, u32>) -> Result<()> {
    let metadata = fs::symlink_metadata(root.join(path))?;
    ensure!(!metadata.file_type().is_symlink(), "storage symlink");
    if metadata.is_dir() {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o777
        };
        #[cfg(not(unix))]
        let mode = 0o755;
        output.insert(path.to_owned(), mode);
        for child in fs::read_dir(root.join(path))? {
            directories(root, &path.join(child?.file_name()), output)?;
        }
    }
    Ok(())
}

pub(super) fn save(root: &Path, output: &mut fs::File, manifest: Manifest) -> Result<()> {
    let workers = hash_workers(manifest.files.len()).clamp(1, MAX_SHARDS);
    let mut groups = vec![BTreeSet::new(); workers];
    let mut weights = vec![0_u64; workers];
    let mut files = Vec::new();
    for path in manifest.files.keys() {
        let metadata = fs::metadata(root.join(path))?;
        // Sparse linker files should be balanced by bytes actually archived.
        #[cfg(unix)]
        let weight = {
            use std::os::unix::fs::MetadataExt;
            metadata.blocks().saturating_mul(512).max(4096)
        };
        #[cfg(not(unix))]
        let weight = metadata.len().max(4096);
        files.push((weight, path));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    for (weight, path) in files {
        let index = weights
            .iter()
            .enumerate()
            .min_by_key(|(_, weight)| *weight)
            .unwrap()
            .0;
        groups[index].insert(path.clone());
        weights[index] = weights[index].saturating_add(weight);
    }
    let mut dirs = BTreeMap::new();
    for path in &manifest.paths {
        directories(root, path, &mut dirs)?;
    }
    output.write_all(MAGIC)?;
    eprintln!(
        "storage creating {} archive shards concurrently",
        groups.len()
    );
    let shards = {
        let output = Mutex::new(&mut *output);
        std::thread::scope(|scope| {
            let handles = groups
                .into_iter()
                .map(|files| {
                    let output = &output;
                    scope.spawn(move || -> Result<_> {
                        let writer = ExtentWriter {
                            output,
                            extents: Vec::new(),
                        };
                        let encoder = zstd::stream::Encoder::new(
                            BufWriter::with_capacity(4 * 1024 * 1024, writer),
                            1,
                        )?;
                        let mut archive = tar::Builder::new(encoder);
                        archive.sparse(true);
                        archive.follow_symlinks(false);
                        for file in &files {
                            archive.append_path_with_name(root.join(file), file)?;
                        }
                        let mut buffered = archive.into_inner()?.finish()?;
                        buffered.flush()?;
                        let writer = buffered.into_inner().map_err(|error| error.into_error())?;
                        Ok(Shard {
                            extents: writer.extents,
                            files,
                        })
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("archive worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?
    };
    let index = Index {
        manifest,
        directories: dirs,
        shards,
    };
    let header = serde_json::to_vec(&index)?;
    ensure!(header.len() as u64 <= MAX_HEADER, "oversized storage index");
    output.write_all(&header)?;
    output.write_all(&(header.len() as u64).to_le_bytes())?;
    output.write_all(MAGIC)?;
    Ok(())
}

/// Coalesce compressed output before serializing writes into the final file.
/// Each worker records only its own extents; no second archive-sized copy exists.
struct ExtentWriter<'a, 'b> {
    output: &'a Mutex<&'b mut fs::File>,
    extents: Vec<(u64, u64)>,
}
impl Write for ExtentWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| std::io::Error::other("archive writer poisoned"))?;
        let offset = output.stream_position()?;
        output.write_all(bytes)?;
        if !bytes.is_empty() {
            self.extents.push((offset, bytes.len() as u64));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct ExtentReader<'a> {
    input: fs::File,
    extents: std::slice::Iter<'a, (u64, u64)>,
    remaining: u64,
}
impl Read for ExtentReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        while self.remaining == 0 {
            let Some(&(offset, length)) = self.extents.next() else {
                return Ok(0);
            };
            self.input.seek(SeekFrom::Start(offset))?;
            self.remaining = length;
        }
        let size = bytes.len().min(self.remaining as usize);
        let read = self.input.read(&mut bytes[..size])?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

pub(super) fn unpack(path: &Path, stage: &Path, key: &str, paths: &[PathBuf]) -> Result<()> {
    let started = std::time::Instant::now();
    let mut input = fs::File::open(path)?;
    let size = input.metadata()?.len();
    ensure!(size >= 24, "truncated storage snapshot");
    input.seek(SeekFrom::End(-16))?;
    let mut footer = [0; 16];
    input.read_exact(&mut footer)?;
    ensure!(&footer[8..] == MAGIC, "invalid storage footer");
    let length = u64::from_le_bytes(footer[..8].try_into()?);
    ensure!(
        length <= MAX_HEADER && length <= size - 24,
        "invalid storage index length"
    );
    let index_start = size - 16 - length;
    input.seek(SeekFrom::Start(index_start))?;
    let index: Index = serde_json::from_reader((&mut input).take(length))?;
    ensure!(
        input.stream_position()? == size - 16,
        "storage index length mismatch"
    );
    ensure!(
        index.manifest.version == 2 && index.manifest.key == key && index.manifest.paths == paths,
        "storage identity or paths mismatch"
    );
    ensure!(
        !index.shards.is_empty() && index.shards.len() <= MAX_SHARDS,
        "invalid shard count"
    );
    let mut seen = BTreeSet::new();
    let mut extents = Vec::new();
    for shard in &index.shards {
        ensure!(!shard.extents.is_empty(), "empty storage shard");
        extents.extend(shard.extents.iter().copied());
        for file in &shard.files {
            relative(file)?;
            ensure!(
                paths.iter().any(|path| file.starts_with(path)),
                "undeclared storage output"
            );
            ensure!(seen.insert(file.clone()), "duplicate storage file");
        }
    }
    extents.sort_unstable();
    let mut end = 8_u64;
    for (offset, length) in extents {
        ensure!(
            offset == end && length > 0,
            "overlapping or missing storage extent"
        );
        end = end.checked_add(length).context("storage extent overflow")?;
        ensure!(end <= index_start, "storage extent leaves payload");
    }
    ensure!(end == index_start, "storage extent length mismatch");
    ensure!(
        seen == index.manifest.files.keys().cloned().collect(),
        "storage file list mismatch"
    );
    // Establish a collision-free directory tree before workers write files.
    for directory in index.directories.keys() {
        relative(directory)?;
        ensure!(
            paths.iter().any(|path| directory.starts_with(path)),
            "undeclared storage directory"
        );
        ensure!(!seen.contains(directory), "file/directory collision");
    }
    for file in &seen {
        let mut parent = file.parent();
        while let Some(directory) = parent.filter(|p| !p.as_os_str().is_empty()) {
            ensure!(!seen.contains(directory), "file ancestor collision");
            parent = directory.parent();
        }
    }
    let mut directories = index.directories.keys().cloned().collect::<BTreeSet<_>>();
    directories.extend(
        seen.iter()
            .filter_map(|file| file.parent())
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_owned),
    );
    for directory in directories {
        fs::create_dir_all(stage.join(directory))?;
    }
    eprintln!(
        "storage extracting {} archive shards concurrently",
        index.shards.len()
    );
    // The on-disk index may have more shards than this runner has CPUs.
    parallel_map(&index.shards, |shard| {
        let reader = ExtentReader {
            input: fs::File::open(path)?,
            extents: shard.extents.iter(),
            remaining: 0,
        };
        let decoder = zstd::stream::Decoder::new(BufReader::with_capacity(1024 * 1024, reader))?;
        let mut archive = tar::Archive::new(decoder);
        let mut extracted = BTreeSet::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let file = entry.path()?.into_owned();
            ensure!(shard.files.contains(&file), "undeclared shard entry");
            ensure!(extracted.insert(file), "duplicate shard entry");
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() || kind.is_gnu_sparse(),
                "storage links and special files are forbidden"
            );
            ensure!(entry.unpack_in(stage)?, "invalid storage entry");
        }
        ensure!(extracted == shard.files, "incomplete storage shard");
        Ok(())
    })?;
    phase("extract", started);
    verify(stage, &index.manifest, paths)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (directory, mode) in index.directories.iter().rev() {
            fs::set_permissions(
                stage.join(directory),
                fs::Permissions::from_mode(mode & 0o777),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_shards_preserve_outputs_and_remove_staging() -> Result<()> {
        let producer = tempfile::tempdir()?;
        let consumer = tempfile::tempdir()?;
        let objects = tempfile::tempdir()?;
        let store = Store {
            directory: Some(objects.path().to_owned()),
            s3: None,
        };
        fs::create_dir(producer.path().join("dist"))?;
        for index in 0..16 {
            fs::write(
                producer.path().join(format!("dist/{index}")),
                format!("payload {index}"),
            )?;
        }
        fs::create_dir(consumer.path().join("dist"))?;
        fs::write(consumer.path().join("dist/original"), "retained")?;
        super::super::save(producer.path(), &store, "malformed", &["dist".into()])?;
        let object = objects
            .path()
            .join(format!("{}.tar", object_key("malformed")?));
        let original = fs::read(&object)?;
        let footer = original.len() - 16;
        let header_length = u64::from_le_bytes(original[footer..footer + 8].try_into()?) as usize;
        let header_start = footer - header_length;
        let payload = &original[8..header_start];
        for failure in [
            "length",
            "duplicate",
            "ownership",
            "traversal",
            "corruption",
            "checksum",
            "directory-collision",
        ] {
            let mut index: Index = serde_json::from_slice(&original[header_start..footer])?;
            match failure {
                "length" => index.shards[0].extents[0].1 += 1,
                "duplicate" if index.shards.len() > 1 => {
                    let duplicate = index.shards[1].files.first().unwrap().clone();
                    index.shards[0].files.insert(duplicate);
                }
                "duplicate" => continue,
                "ownership" if index.shards.len() > 1 => {
                    let left = index.shards[0].files.pop_first().unwrap();
                    let right = index.shards[1].files.pop_first().unwrap();
                    index.shards[0].files.insert(right);
                    index.shards[1].files.insert(left);
                }
                "ownership" => continue,
                "traversal" => {
                    index.directories.insert("../outside".into(), 0o755);
                }
                "checksum" => {
                    index.manifest.files.values_mut().next().unwrap().digest =
                        "incorrect checksum".into()
                }
                "directory-collision" => {
                    index.directories.insert("dist/0".into(), 0o755);
                }
                "corruption" => (),
                _ => unreachable!(),
            }
            let header = serde_json::to_vec(&index)?;
            let mut modified = MAGIC.to_vec();
            modified.extend(payload);
            modified.extend(header);
            modified.extend((modified.len() as u64 - header_start as u64).to_le_bytes());
            modified.extend(MAGIC);
            if failure == "corruption" {
                modified[8] ^= 0xff;
            }
            fs::write(&object, modified)?;
            assert!(
                restore(
                    consumer.path(),
                    &store,
                    &["malformed".into()],
                    &["dist".into()],
                    true
                )
                .is_err(),
                "accepted {failure}"
            );
            assert_eq!(
                fs::read(consumer.path().join("dist/original"))?,
                b"retained"
            );
            assert_eq!(
                fs::read_dir(consumer.path())?.count(),
                1,
                "staging leaked on {failure}"
            );
        }
        Ok(())
    }

    #[test]
    fn restores_legacy_snapshots_without_overwriting_their_object() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("dist/empty"))?;
        fs::write(root.path().join("dist/output"), "legacy")?;
        let manifest = Manifest {
            version: 1,
            key: "shared".into(),
            paths: vec!["dist".into()],
            files: BTreeMap::from([(
                "dist/output".into(),
                file_state(&root.path().join("dist/output"))?,
            )]),
        };
        let object = root.path().join(format!(
            ".flux/cache/{}.tar",
            object_key("shared")?.replace("objects/v2/", "objects/")
        ));
        fs::create_dir_all(object.parent().unwrap())?;
        let mut archive =
            tar::Builder::new(zstd::stream::Encoder::new(fs::File::create(&object)?, 1)?);
        let bytes = serde_json::to_vec(&manifest)?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, MANIFEST, &bytes[..])?;
        archive.append_dir_all("dist", root.path().join("dist"))?;
        archive.into_inner()?.finish()?;
        let legacy_bytes = fs::read(&object)?;
        fs::remove_dir_all(root.path().join("dist"))?;
        assert!(restore(
            root.path(),
            &Store::default(),
            &["shared".into()],
            &["dist".into()],
            true
        )?);
        assert_eq!(fs::read(root.path().join("dist/output"))?, b"legacy");
        fs::write(root.path().join("dist/output"), "parallel")?;
        super::super::save(root.path(), &Store::default(), "shared", &["dist".into()])?;
        assert_eq!(fs::read(&object)?, legacy_bytes);
        fs::remove_dir_all(root.path().join("dist"))?;
        restore(
            root.path(),
            &Store::default(),
            &["shared".into()],
            &["dist".into()],
            true,
        )?;
        assert_eq!(fs::read(root.path().join("dist/output"))?, b"parallel");
        assert!(root.path().join("dist/empty").is_dir());
        Ok(())
    }
}
