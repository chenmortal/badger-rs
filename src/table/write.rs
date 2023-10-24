use std::{
    sync::{atomic::{AtomicU32, Ordering}, Arc}, mem::replace,
};

use bytes::{Buf, BufMut};
use prost::Message;

use crate::{
    iter::{KvSinkIterator, SinkIterator},
    kv::{ KeyTsBorrow, ValuePointer},
    options::CompressionType,
    txn::{entry::{EntryMeta, ValueMeta}, TxnTs}, key_registry::NONCE_SIZE, bloom::Bloom, pb::badgerpb4::{Checksum, checksum::Algorithm}, rayon::{spawn_fifo, AsyncRayonHandle},
};

use super::{TableOption, vec_u32_to_bytes, try_encrypt};
#[derive(Debug)]
pub(crate) struct EntryHeader {
    overlap: u16,
    diff: u16,
}
// Header + base_key (diff bytes)
pub(crate) const HEADER_SIZE: usize = 4;
impl EntryHeader {
    pub(crate) fn new(overlap:u16,diff:u16)->Self{
        Self{
            overlap,
            diff,
        }
    }
    #[inline]
    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER_SIZE);
        v.put_u16(self.overlap);
        v.put_u16(self.diff);
        v
    }
    #[inline]
    pub(crate) fn deserialize(mut data: &[u8]) -> Self {
        
        Self {
            overlap: data.get_u16(),
            diff: data.get_u16(),
        }
    }
    #[inline]
    pub(crate) fn get_diff(&self) -> usize {
        self.diff as usize
    }
    #[inline]
    pub(crate) fn get_overlap(&self) -> usize {
        self.overlap as usize
    }
}
#[derive(Debug, Default)]
struct BackendBlock {
    data: Vec<u8>,
    basekey: Vec<u8>,
    entry_offsets: Vec<u32>,
}
impl BackendBlock {
    fn new(block_size: usize) -> Self {
        Self {
            data: Vec::with_capacity(block_size + BLOCK_PADDING),
            basekey: Default::default(),
            entry_offsets: Default::default(),
        }
    }
}


#[derive(Debug, Default)]
pub(crate) struct TableBuilder {
    alloc: Vec<u8>,
    cur_block: BackendBlock,
    compressed_size: Arc<AtomicU32>,
    uncompressed_size: u32,
    len_offsets: u32,
    key_hashes: Vec<u32>,
    max_version: TxnTs,
    on_disk_size: u32,
    stale_data_size: usize,
    opt: TableOption,
    compress_task:Vec<AsyncRayonHandle<anyhow::Result<BackendBlock>>>
}
const MAX_BUFFER_BLOCK_SIZE: usize = 256 << 20; //256MB
/// When a block is encrypted, it's length increases. We add 256 bytes of padding to
/// handle cases when block size increases. This is an approximate number.
const BLOCK_PADDING: usize = 256;
impl BackendBlock {
    fn len(&self)->usize{
        self.data.len()
    }
    fn diff_base_key(&self,new_key:&[u8])->usize{
        let mut i=0;
        let base_key:&[u8]=self.basekey.as_ref();
        while i < base_key.len().min(new_key.len()) {
            if base_key[i]!=new_key[i]{
                break;
            }
            i+=1;
        }
        i
    }
    fn should_finish_block(&self, key: &KeyTsBorrow, value: &ValueMeta,block_size:usize,is_encrypt:bool) -> bool {
        if self.entry_offsets.len() == 0 {
            return false;
        }
        debug_assert!((self.entry_offsets.len() as u32 + 1) * 4 + 4 + 8 + 4 < u32::MAX);
        let entries_offsets_size = (self.entry_offsets.len() + 1) * 4 
        + 4 //size of list
        + 8 //sum64 in checksum proto
        + 4; //checksum length
        let mut estimate_size=self.data.len()+6+key.as_ref().len()+ value.encode_size().unwrap() as usize+ entries_offsets_size;
        if is_encrypt{
            estimate_size+=NONCE_SIZE;
        }
        assert!(self.data.len()+estimate_size < u32::MAX as usize);
        estimate_size > block_size
    }

    fn push_entry(&mut self,key_ts: &KeyTsBorrow,value: ValueMeta){
        let diff_key=if self.basekey.len()==0 {
            self.basekey=key_ts.to_vec();
            key_ts
        }else{
            &key_ts[self.diff_base_key(&key_ts)..]
        };
        assert!(key_ts.len()-diff_key.len() <= u16::MAX as usize);
        assert!(diff_key.len() <= u16::MAX as usize);
        let entry_header=EntryHeader::new((key_ts.len()-diff_key.len()) as u16, diff_key.len() as u16);
        self.entry_offsets.push(self.data.len() as u32);
        self.data.extend_from_slice(&entry_header.serialize());
        self.data.extend_from_slice(diff_key);
        self.data.extend_from_slice(value.serialize().unwrap().as_ref());
        
    }
    fn finish_block(&mut self,algo:Algorithm){
        self.data.extend_from_slice(&vec_u32_to_bytes(&self.entry_offsets));
        self.data.put_u32(self.entry_offsets.len() as u32);

        let checksum = Checksum::new(algo, &self.data);
        self.data.extend_from_slice(&checksum.encode_to_vec());
        self.data.put_u32(checksum.encoded_len() as u32);
    }
}
impl TableBuilder {
    fn new(opt: TableOption) -> Self {
        let pre_alloc_size = MAX_BUFFER_BLOCK_SIZE.min(opt.table_size());
        let cur_block = BackendBlock::new(opt.block_size());
        let mut table_builder = Self::default();
        table_builder.cur_block = cur_block;
        table_builder.alloc = Vec::with_capacity(pre_alloc_size);
        table_builder.opt = opt;
        table_builder
    }
    fn push_internal(&mut self, key_ts: &KeyTsBorrow, value: ValueMeta,vptr_len:Option<u32>, is_stale: bool) {
        if self.cur_block.should_finish_block(&key_ts, &value,self.opt.block_size(),self.opt.cipher().is_some()) {
            if is_stale{
                self.stale_data_size+=key_ts.len()+4;
            }
            self.finish_cur_block();
        };
        self.key_hashes.push(Bloom::hash(key_ts.key()));
        self.max_version=self.max_version.max(key_ts.txn_ts());
        self.cur_block.push_entry(key_ts, value);
        self.on_disk_size+=vptr_len.unwrap_or(0);
    }
     fn finish_cur_block(&mut self){
        if self.cur_block.entry_offsets.len()==0 {
            return;
        }
        self.cur_block.finish_block(self.opt.block_checksum_algo());
        self.uncompressed_size+=self.cur_block.len() as u32;

        self.len_offsets+=(self.cur_block.basekey.len() as f32/ 4.0).ceil() as u32 * 4 + 40;
        let mut finished_block = replace(&mut self.cur_block, BackendBlock::new(self.opt.block_size()));
        let cipher = self.opt.cipher_clone();
        let compression = self.opt.compression();
        let compressed_size = self.compressed_size.clone();
        self.compress_task.push(spawn_fifo(move ||{
                    if compression!=CompressionType::None{
                        match compression.compress(&finished_block.data) {
                            Ok(compressed) => {
                                finished_block.data=compressed;
                            },
                            Err(e) => {
                                return Err(e);
                            },
                        }
                    }
                    if let Some(cipher) = cipher.as_ref() {
                        match try_encrypt(cipher.into(), &finished_block.data) {
                            Ok(ciphertext) => {
                                finished_block.data=ciphertext;
                            },
                            Err(e) => {return Err(e)},
                        }
                    }
                    compressed_size.fetch_add(finished_block.len() as u32, Ordering::AcqRel);
                    Ok(finished_block)
                }));
    }
    fn push(&mut self,key_ts: &KeyTsBorrow,value: ValueMeta,vptr_len:Option<u32>){
        self.push_internal(key_ts, value, vptr_len, false);
    }
    pub(crate) fn build_l0_table<'a, I, K, V>(
        mut iter: I,
        drop_prefixed: Vec<Vec<u8>>,
        opt: TableOption,
    ) -> anyhow::Result<Self>
    where
        I: KvSinkIterator<'a, K, V> + SinkIterator,
        K: Into<KeyTsBorrow<'a>>,
        V: Into<ValueMeta>,
    {
        let mut table_builder = Self::new(opt);
        while iter.next()? {
            let key_ts: KeyTsBorrow = iter.key().unwrap().into();
            let key_bytes = key_ts.as_ref();
            if drop_prefixed.len() > 0 && drop_prefixed.iter().any(|x| key_bytes.starts_with(x)) {
                continue;
            }
            let value: ValueMeta = iter.take_value().unwrap().into();
            let vptr_len=if value.meta().contains(EntryMeta::VALUE_POINTER) {
                let vp = ValuePointer::decode(&value.value());
                vp.len().into()
            }else {
                None
            };
            table_builder.push(&key_ts, value, vptr_len);
        }
        Ok(table_builder)
    }
    pub(crate) fn is_empty(&self)->bool{
        self.key_hashes.len()==0
    }
    pub(crate) fn finish(){

    }
    pub(crate) async fn done(&mut self)->anyhow::Result<()>{
        self.finish_cur_block();
        let mut block_list=Vec::with_capacity(self.compress_task.len());
        for task in self.compress_task.drain(..) {
            block_list.push(task.await?);
        } 
          
        Ok(())
    }
}

