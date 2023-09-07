use std::sync::Arc;
#[derive(Debug)]
pub(crate) struct Block(Arc<BlockInner>);
#[derive(Debug)]
struct BlockInner{
    offset:i32,
    data:Vec<u8>,
    checksum:Vec<u8>,
    entries_index_start:i32,
    entryoffset:Vec<u32>,
    checksum_len:u32,
}