use std::{mem::size_of, sync::Arc, time::SystemTime};

use crate::{
    db::{BlockCache, IndexCache},
    key_registry::{AesCipher, KeyRegistry},
    options::{CompressionType, Options},
    pb::badgerpb4::{self, checksum::Algorithm, DataKey},
};
// ChecksumVerificationMode tells when should DB verify checksum for SSTable blocks.
#[derive(Debug, Clone, Copy)]
pub enum ChecksumVerificationMode {
    // NoVerification indicates DB should not verify checksum for SSTable blocks.
    NoVerification,

    // OnTableRead indicates checksum should be verified while opening SSTtable.
    OnTableRead,

    // OnBlockRead indicates checksum should be verified on every SSTable block read.
    OnBlockRead,

    // OnTableAndBlockRead indicates checksum should be verified
    // on SSTable opening and on every block read.
    OnTableAndBlockRead,
}
impl Default for ChecksumVerificationMode {
    fn default() -> Self {
        Self::NoVerification
    }
}
#[derive(Debug, Clone)]
pub(crate) struct TableOption {
    // Open tables in read only mode.
    // Maximum size of the table.
    table_size: usize,
    table_capacity: u64, // 0.9x TableSize.

    // ChkMode is the checksum verification mode for Table.
    checksum_verify_mode: ChecksumVerificationMode,
    block_checksum_algo: badgerpb4::checksum::Algorithm,
    // BloomFalsePositive is the false positive probabiltiy of bloom filter.
    bloom_false_positive: f64,

    // BlockSize is the size of each block inside SSTable in bytes.
    block_size: usize,

    // DataKey is the key used to decrypt the encrypted text.
    // pub(crate) datakey: Option<DataKey>,
    cipher: Arc<Option<AesCipher>>,

    // Compression indicates the compression algorithm used for block compression.
    compression: CompressionType,

    zstd_compression_level: i32,
    block_cache: Option<BlockCache>,

    index_cache: Option<IndexCache>,
}
impl Default for TableOption {
    fn default() -> Self {
        Self {
            table_size: 2 << 20,
            table_capacity: Default::default(),
            checksum_verify_mode: Default::default(),
            bloom_false_positive: 0.01,
            block_size: 4 * 1024,
            compression: Default::default(),
            zstd_compression_level: 1,
            block_cache: Default::default(),
            index_cache: Default::default(),
            block_checksum_algo: Default::default(),
            cipher: None.into(),
        }
    }
}
impl TableOption {
    pub(crate) async fn new(
        key_registry: &KeyRegistry,
        block_cache: &Option<BlockCache>,
        index_cache: &Option<IndexCache>,
    ) -> Self {
        let mut registry_w = key_registry.write().await;
        let cipher = registry_w.latest_cipher().await.into();
        drop(registry_w);
        Self {
            table_capacity: (Options::base_table_size() as f64 * 0.95) as u64,
            bloom_false_positive: Options::bloom_false_positive(),
            block_size: Options::block_size(),
            compression: Options::compression(),
            zstd_compression_level: Options::zstd_compression_level(),
            block_cache: block_cache.clone(),
            index_cache: index_cache.clone(),
            table_size: Options::base_table_size(),
            checksum_verify_mode: Options::checksum_verification_mode(),
            block_checksum_algo: Options::block_checksum_algo(),
            cipher,
        }
    }

    pub(crate) fn block_cache(&self) -> Option<&BlockCache> {
        self.block_cache.as_ref()
    }

    pub(crate) fn block_size(&self) -> usize {
        self.block_size
    }

    pub(crate) fn checksum_verify_mode(&self) -> ChecksumVerificationMode {
        self.checksum_verify_mode
    }

    pub(crate) fn table_size(&self) -> usize {
        self.table_size
    }

    pub(crate) fn set_table_size(&mut self, table_size: usize) {
        self.table_size = table_size;
        self.table_capacity = (self.table_size as f64 * 0.95) as u64;
    }

    pub(crate) fn zstd_compression_level(&self) -> i32 {
        self.zstd_compression_level
    }

    pub(crate) fn block_checksum_algo(&self) -> Algorithm {
        self.block_checksum_algo
    }

    pub(crate) fn cipher(&self) -> Option<&AesCipher> {
        self.cipher.as_ref().as_ref()
    }
    pub(crate) fn cipher_clone(&self) -> Arc<Option<AesCipher>> {
        self.cipher.clone()
    }

    pub(crate) fn set_cipher_with_key(&mut self, cipher: Option<DataKey>) {
        if let Some(key) = cipher {
            if let Ok(cipher) = AesCipher::new(&key.data, Options::aes_is_siv()) {
                self.cipher = Arc::new(cipher.into())
            }
        }
    }

    pub(crate) fn compression(&self) -> CompressionType {
        self.compression
    }

    pub(crate) fn set_compression(&mut self, compression: CompressionType) {
        self.compression = compression;
    }
}
#[test]
fn test_size() {
    let key = [0u8; 16];
    let aes = Some(AesCipher::new(key.as_ref(), true).unwrap());
    let start = SystemTime::now();
    for _i in 0..10000 {
        let aes = aes.clone();
    }
    let p = SystemTime::now().duration_since(start).unwrap();
    dbg!(p);
}
#[test]
fn test_a() {
    let key = [0u8; 16];
    let aes = Arc::new(Some(AesCipher::new(key.as_ref(), true).unwrap()));
    let start = SystemTime::now();
    for _i in 0..1_000_000 {
        let aes = aes.clone();
    }
    let p = SystemTime::now().duration_since(start).unwrap();
    dbg!(p);
}
#[test]
fn test_b() {
    let key = [0u8; 16];
    let aes = Some(Arc::new(AesCipher::new(key.as_ref(), true).unwrap()));
    let start = SystemTime::now();
    for _i in 0..1_000_000 {
        let aes = aes.clone();
    }
    let p = SystemTime::now().duration_since(start).unwrap();
    dbg!(p);
}
