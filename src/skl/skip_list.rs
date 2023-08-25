use std::sync::atomic::{AtomicI32, AtomicU64,AtomicU32};
use crate::default::SKL_MAX_HEIGHT;
pub(crate) struct Node{
    value:AtomicU64,
    key_offset:u32,
    key_size:u16,
    height:u16,
    tower:[AtomicU32;SKL_MAX_HEIGHT]
}
pub(crate) struct SkipList{
    height:AtomicI32,
    head:Node,
    
}
impl SkipList {
    fn new(arena_size:u64){
        
    }
}