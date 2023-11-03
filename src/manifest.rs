use std::{
    collections::{HashMap, HashSet},
    fs::{rename, File, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{anyhow, bail};
use bytes::{Buf, BufMut};
use parking_lot::Mutex;
use prost::Message;

use crate::{
    default::{DEFAULT_DIR, MANIFEST_FILE_NAME, MANIFEST_REWRITE_FILE_NAME},
    errors::err_file,
    config::CompressionType,
    pb::badgerpb4::{manifest_change, ManifestChange, ManifestChangeSet},
    util::{sys::sync_dir, SSTableId},
};
#[derive(Debug, Default, Clone)]
pub(crate) struct Manifest {
    levels: Vec<LevelManifest>,
    pub(crate) tables: HashMap<SSTableId, TableManifest>,
    creations: isize,
    deletions: isize,
}
#[derive(Debug, Default, Clone)]
struct LevelManifest {
    tables: HashSet<u64>, //Set of table id's
}
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TableManifest {
    pub(crate) level: u8,
    pub(crate) keyid: u64,
    pub(crate) compression: CompressionType,
}
#[derive(Debug)]
pub(crate) struct ManifestFile {
    file_handle: File,
    pub(crate) manifest: Arc<Mutex<Manifest>>,
}
#[derive(Debug, Clone)]
pub struct ManifestConfig {
    dir: PathBuf,
    read_only: bool,
    // Magic version used by the application using badger to ensure that it doesn't open the DB
    // with incompatible data format.
    external_magic_version: u16,
}
impl Default for ManifestConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(DEFAULT_DIR),
            read_only: false,
            external_magic_version: 0,
        }
    }
}
impl ManifestConfig {
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
    }
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn set_external_magic_version(&mut self, external_magic_version: u16) {
        self.external_magic_version = external_magic_version;
    }
    pub(crate) fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }
    pub(crate) fn build(&self) -> anyhow::Result<ManifestFile> {
        let path = self.dir.join(MANIFEST_FILE_NAME);
        match OpenOptions::new()
            .read(true)
            .write(!self.read_only)
            .open(&path)
        {
            Ok(mut file_handle) => {
                let (manifest, trunc_offset) =
                    replay_manifest_file(&file_handle, self.external_magic_version)?;
                if !self.read_only {
                    file_handle.set_len(trunc_offset)?;
                }
                file_handle.seek(SeekFrom::End(0))?;
                let manifest_file = ManifestFile {
                    file_handle,
                    manifest: Arc::new(Mutex::new(manifest)),
                };
                Ok(manifest_file)
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => {
                    if self.read_only {
                        bail!(err_file(
                            e,
                            &path,
                            "no manifest found, required for read-only db"
                        ));
                    }
                    let manifest = Manifest::default();
                    let (file_handle, net_creations) = self.help_rewrite(&manifest)?;
                    assert_eq!(net_creations, 0);
                    let manifest_file = ManifestFile {
                        file_handle,
                        manifest: Arc::new(Mutex::new(manifest)),
                    };
                    Ok(manifest_file)
                }
                _ => bail!(e),
            },
        }
    }
    fn help_rewrite(&self, manifest: &Manifest) -> anyhow::Result<(File, usize)> {
        let rewrite_path = self.dir.join(MANIFEST_REWRITE_FILE_NAME);
        let mut fp = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&rewrite_path)?;

        // magic bytes are structured as
        // +---------------------+-------------------------+-----------------------+
        // | magicText (4 bytes) | externalMagic (2 bytes) | badgerMagic (2 bytes) |
        // +---------------------+-------------------------+-----------------------+

        let mut buf = Vec::with_capacity(8);
        buf.put(&MAGIC_TEXT[..]);
        buf.put_u16(self.external_magic_version);
        buf.put_u16(BADGER_MAGIC_VERSION);

        let net_creations = manifest.tables.len();
        let changes = manifest.as_changes();
        let set = ManifestChangeSet { changes };
        let change_set_buf = set.encode_to_vec();

        let mut len_crc_buf = Vec::with_capacity(8);
        len_crc_buf.put_u32(change_set_buf.len() as u32);
        len_crc_buf.put_u32(crc32fast::hash(&change_set_buf));

        buf.extend_from_slice(&len_crc_buf);
        buf.extend_from_slice(&change_set_buf);
        fp.write_all(&buf)?;
        fp.sync_all()?;
        drop(fp);

        let manifest_path = self.dir.join(MANIFEST_FILE_NAME);
        rename(rewrite_path, &manifest_path)?;
        let mut fp = OpenOptions::new()
            .read(true)
            .write(true)
            .open(manifest_path)?;
        fp.seek(SeekFrom::End(0))?;
        sync_dir(&self.dir)?;
        Ok((fp, net_creations))
    }
}

const BADGER_MAGIC_VERSION: u16 = 8;
const MAGIC_TEXT: &[u8; 4] = b"Bdgr";

fn replay_manifest_file(fp: &File, ext_magic: u16) -> anyhow::Result<(Manifest, u64)> {
    let mut reader = BufReader::new(fp);
    let mut magic_buf = [0; 8];
    let mut offset: u64 = 0;
    offset += reader
        .read(&mut magic_buf)
        .map_err(|e| anyhow!("manifest has bad magic : {}", e))? as u64;
    if magic_buf[..4] != MAGIC_TEXT[..] {
        bail!("manifest has bad magic");
    }

    let ext_version = magic_buf.as_ref().get_u16();
    let version = magic_buf.as_ref().get_u16();
    if version != BADGER_MAGIC_VERSION {
        bail!(
            "manifest has upsupported version: {} (we support {} )",
            version,
            BADGER_MAGIC_VERSION
        );
    }
    if ext_version != ext_magic {
        bail!("cannot open db because the external magic number doesn't match. Expected: {}, version present in manifest: {}",ext_magic,ext_version);
    }
    let fp_szie = fp.metadata()?.len();

    let mut manifest = Manifest::default();
    loop {
        let mut read_size = 0;
        let mut len_crc_buf = [0; 8];
        match reader.read_exact(len_crc_buf.as_mut()) {
            Ok(_) => {
                read_size += 8;
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::UnexpectedEof => break,
                _ => bail!(e),
            },
        };

        let mut len_crc_buf_ref = len_crc_buf.as_ref();

        let change_len = len_crc_buf_ref.get_u32() as usize;
        let crc = len_crc_buf_ref.get_u32();
        if (offset + change_len as u64) > fp_szie {
            bail!("buffer len too greater, Manifest file might be corrupted");
        }

        let mut change_set_buf = vec![0 as u8; change_len];
        match reader.read_exact(&mut change_set_buf) {
            Ok(_) => {
                read_size += change_len;
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::UnexpectedEof => break,
                _ => bail!(e),
            },
        };

        if crc32fast::hash(&change_set_buf) != crc {
            bail!("manifest has checksum mismatch");
        }
        offset += read_size as u64;
        let change_set = ManifestChangeSet::decode(change_set_buf.as_ref())?;
        manifest.apply_change_set(&change_set)?;
    }
    Ok((manifest, offset))
}
impl Manifest {
    fn as_changes(&self) -> Vec<ManifestChange> {
        let mut changes = Vec::<ManifestChange>::with_capacity(self.tables.len());
        for (id, table_manifest) in self.tables.iter() {
            changes.push(ManifestChange::new_create_change(
                (*id).into(),
                table_manifest.level as u32,
                table_manifest.keyid,
                table_manifest.compression,
            ));
        }

        changes
    }
    fn apply_change_set(&mut self, change_set: &ManifestChangeSet) -> anyhow::Result<()> {
        for change in change_set.changes.iter() {
            self.apply_manifest_change(change)?;
        }
        Ok(())
    }
    fn apply_manifest_change(&mut self, change: &ManifestChange) -> anyhow::Result<()> {
        match change.op() {
            manifest_change::Operation::Create => {
                if self.tables.get(&change.id.into()).is_some() {
                    bail!("MANIFEST invalid, table {} exists", change.id);
                }
                self.tables.insert(
                    change.id.into(),
                    TableManifest {
                        level: change.level as u8,
                        keyid: change.key_id,
                        compression: CompressionType::from(change.compression),
                    },
                );
                if self.levels.len() <= change.level as usize {
                    self.levels.push(LevelManifest::default());
                }
                self.levels[change.level as usize].tables.insert(change.id.into());
                self.creations += 1;
            }
            manifest_change::Operation::Delete => {
                if self.tables.get(&change.id.into()).is_none() {
                    bail!("MANIFEST removes non-existing table {}", change.id);
                }
                self.levels[change.level as usize].tables.remove(&change.id.into());
                self.tables.remove(&change.id.into());
                self.deletions += 1;
            }
        }
        Ok(())
    }
}
