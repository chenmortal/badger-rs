use std::{
    hash::Hasher,
    io::{BufWriter, Write},
    mem,
    sync::atomic::Ordering,
};

use bytes::BufMut;

use crate::{
    default::DEFAULT_PAGE_SIZE,
    kv::ValuePointer,
    lsm::wal::LogFile,
    txn::{entry::DecEntry, TxnTs},
    vlog::{BIT_FIN_TXN, BIT_TXN},
    write::WriteReq,
};

use super::{header::EntryHeader, ValueLog, MAX_HEADER_SIZE, MAX_VLOG_FILE_SIZE};
use anyhow::bail;
pub(crate) struct HashWriter<'a, T: Hasher> {
    writer: &'a mut Vec<u8>,
    hasher: T,
}

impl<T: Hasher> Write for HashWriter<'_, T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.put_slice(buf);
        self.hasher.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ValueLog {
    async fn write(&mut self, reqs: &mut Vec<WriteReq>) -> anyhow::Result<()> {
        self.validate_write(reqs)?;
        let fid_logfile_r = self.fid_logfile.read().await;
        let log_file = fid_logfile_r.get(&self.max_fid);
        debug_assert!(log_file.is_some());
        let log_file = log_file.unwrap();
        let mut buf = Vec::with_capacity(DEFAULT_PAGE_SIZE.to_owned());

        let write=||{
            if buf.len()==0{
                return;
            }

        };
        for req in reqs.iter_mut() {
            let entries_vptrs = req.entries_vptrs_mut();
            let mut value_sizes = Vec::with_capacity(entries_vptrs.len());

            for (dec_entry, vptr) in entries_vptrs {
                buf.clear();
                value_sizes.push(dec_entry.value().len());
                dec_entry.try_set_value_threshold(self.threshold.value_threshold());
                if dec_entry.value().len() < dec_entry.value_threshold() {
                    if !vptr.is_empty() {
                        *vptr = ValuePointer::default();
                    }
                    continue;
                }
                let fid = log_file.fid();
                let offset = self.writable_log_offset();

                let tmp_meta = dec_entry.meta();
                dec_entry.clean_meta_bit(BIT_TXN | BIT_FIN_TXN);

                let len = log_file.encode_entry(&mut buf, &dec_entry, offset);

                dec_entry.set_meta(tmp_meta);
                *vptr = ValuePointer::new(fid, len, offset);


            }
        }
        Ok(())
    }
    fn validate_write(&self, reqs: &Vec<WriteReq>) -> anyhow::Result<()> {
        let mut vlog_offset = self.writable_log_offset();
        for req in reqs {
            let mut size = 0;
            req.entries_vptrs().iter().for_each(|(x, _)| {
                size += MAX_HEADER_SIZE
                    + x.entry.key().len()
                    + mem::size_of::<TxnTs>()
                    + x.entry.value().len()
                    + mem::size_of::<u32>()
            });
            let estimate = vlog_offset + size;
            if estimate > MAX_VLOG_FILE_SIZE {
                bail!(
                    "Request size offset {} is bigger than maximum offset {}",
                    estimate,
                    MAX_VLOG_FILE_SIZE
                )
            }

            if estimate >= self.opt.vlog_file_size {
                vlog_offset = 0;
                continue;
            }
            vlog_offset = estimate;
        }
        Ok(())
    }
    #[inline]
    pub(crate) fn writable_log_offset(&self) -> usize {
        self.writable_log_offset.load(Ordering::SeqCst)
    }
    #[inline]
    pub(crate) fn writable_log_offset_fetch_add(&self,size:usize)->usize{
        self.writable_log_offset.fetch_add(size, Ordering::SeqCst)
    }
}
impl LogFile {
    fn encode_entry(&self, buf: &mut Vec<u8>, entry: &DecEntry, offset: usize) -> usize {
        let header = EntryHeader::new(&entry);
        let mut hash_writer = HashWriter {
            writer: buf,
            hasher: crc32fast::Hasher::new(),
        };
        let header_encode = header.encode();
        let header_len = hash_writer.write(&header_encode).unwrap();

        let mut kv_buf = entry.key_ts().get_bytes();
        kv_buf.extend_from_slice(entry.value());
        if let Some(e) = self.try_encrypt(&kv_buf, offset) {
            kv_buf = e;
        };
        let kv_len = hash_writer.write(&kv_buf).unwrap();

        let crc = hash_writer.hasher.finalize();
        let buf = hash_writer.writer;
        buf.put_u32(crc);
        header_len + kv_len + mem::size_of::<u32>()
    }
}
