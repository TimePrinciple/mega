//!
//!
//!
//!
//!
//!
use std::io::{self, BufRead, Cursor, ErrorKind, Read, Seek};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle, sleep};
use std::time::Instant;

use flate2::bufread::ZlibDecoder;
use threadpool::ThreadPool;

use venus::errors::GitError;
use venus::hash::SHA1;
use venus::internal::object::types::ObjectType;

use super::cache::_Cache;
use crate::internal::pack::cache::Caches;
use crate::internal::pack::cache_object::{CacheObject, MemSizeRecorder};
use crate::internal::pack::waitlist::Waitlist;
use crate::internal::pack::wrapper::Wrapper;
use crate::internal::pack::{utils, Pack};
use uuid::Uuid;
use venus::internal::pack::entry::Entry;

/// For Convenient to pass Params
struct SharedParams {
    pub pool: Arc<ThreadPool>,
    pub waitlist: Arc<Waitlist>,
    pub caches: Arc<Caches>,
    pub cache_objs_mem_size: Arc<AtomicUsize>,
    pub callback: Arc<dyn Fn(Entry) + Sync + Send>
}

impl Pack {
    /// # Parameters
    /// - `thread_num`: The number of threads to use for decoding and cache, `None` mean use the number of logical CPUs.
    /// It can't be zero, or panic <br>
    /// - `mem_limit`: The maximum size of the memory cache in bytes, or None for unlimited.
    /// The 80% of it will be used for [Caches]  <br>
    ///     **Not very accurate, because of memory alignment and other reasons, overuse about 15%** <br>
    /// - `temp_path`: The path to a directory for temporary files, default is "./.cache_temp" <br>
    /// For example, thread_num = 4 will use up to 8 threads (4 for decoding and 4 for cache) <br>
    ///
    /// # !IMPORTANT:
    /// Can't decode in multi-tasking, because memory limit use shared static variable but different cache, cause "deadlock".
    pub fn new(thread_num: Option<usize>, mem_limit: Option<usize>, temp_path: Option<PathBuf>) -> Self {
        let mut temp_path = temp_path.unwrap_or(PathBuf::from("./.cache_temp"));
        temp_path.push(Uuid::new_v4().to_string()); //maybe Snowflake or ULID is better (less collision)
        let thread_num = thread_num.unwrap_or_else(num_cpus::get);
        let cache_mem_size = mem_limit.map(|mem_limit| mem_limit * 4 / 5);
        Pack {
            number: 0,
            signature: SHA1::default(),
            objects: Vec::new(),
            pool: Arc::new(ThreadPool::new(thread_num)),
            waitlist: Arc::new(Waitlist::new()),
            caches:  Arc::new(Caches::new(cache_mem_size, temp_path, thread_num)),
            mem_limit: mem_limit.unwrap_or(usize::MAX),
            cache_objs_mem: Arc::new(AtomicUsize::default()),
        }
    }

    /// Checks and reads the header of a Git pack file.
    ///
    /// This function reads the first 12 bytes of a pack file, which include the "PACK" magic identifier,
    /// the version number, and the number of objects in the pack. It verifies that the magic identifier
    /// is correct and that the version number is 2 (which is the version currently supported by Git).
    /// It also collects these header bytes for later use, such as for hashing the entire pack file.
    ///
    /// # Parameters
    /// * `pack`: A mutable reference to an object implementing the `Read` trait,
    ///           representing the source of the pack file data (e.g., file, memory stream).
    ///
    /// # Returns
    /// A `Result` which is:
    /// * `Ok((u32, Vec<u8>))`: On successful reading and validation of the header, returns a tuple where:
    ///     - The first element is the number of objects in the pack file (`u32`).
    ///     - The second element is a vector containing the bytes of the pack file header (`Vec<u8>`).
    /// * `Err(GitError)`: On failure, returns a `GitError` with a description of the issue.
    ///
    /// # Errors
    /// This function can return an error in the following situations:
    /// * If the pack file does not start with the "PACK" magic identifier.
    /// * If the pack file's version number is not 2.
    /// * If there are any issues reading from the provided `pack` source.
    pub fn check_header(pack: &mut (impl Read + BufRead)) -> Result<(u32, Vec<u8>), GitError> {
        // A vector to store the header data for hashing later
        let mut header_data = Vec::new();

        // Read the first 4 bytes which should be "PACK"
        let mut magic = [0; 4];
        // Read the magic "PACK" identifier
        let result = pack.read_exact(&mut magic);
        match result {
            Ok(_) => {
                // Store these bytes for later
                header_data.extend_from_slice(&magic);

                // Check if the magic bytes match "PACK"
                if magic != *b"PACK" {
                    // If not, return an error indicating invalid pack header
                    return Err(GitError::InvalidPackHeader(format!(
                        "{},{},{},{}",
                        magic[0], magic[1], magic[2], magic[3]
                    )));
                }
            },
            Err(_e) => {
                // If there is an error in reading, return a GitError
                return Err(GitError::InvalidPackHeader(format!(
                    "{},{},{},{}",
                    magic[0], magic[1], magic[2], magic[3]
                )));
            }
        }

        // Read the next 4 bytes for the version number
        let mut version_bytes = [0; 4];
        let result = pack.read_exact(&mut version_bytes); // Read the version number
        match result {
            Ok(_) => {
                // Store these bytes
                header_data.extend_from_slice(&version_bytes);

                // Convert the version bytes to an u32 integer
                let version = u32::from_be_bytes(version_bytes);
                if version != 2 {
                    // Git currently supports version 2, so error if not version 2
                    return Err(GitError::InvalidPackFile(format!(
                        "Version Number is {}, not 2",
                        version
                    )));
                }
                // If read is successful, proceed
            },
            Err(_e) => {
                // If there is an error in reading, return a GitError
                return Err(GitError::InvalidPackHeader(format!(
                    "{},{},{},{}",
                    version_bytes[0], version_bytes[1], version_bytes[2], version_bytes[3]
                )));
            }
        }

        // Read the next 4 bytes for the number of objects in the pack
        let mut object_num_bytes = [0; 4];
        // Read the number of objects
        let result = pack.read_exact(&mut object_num_bytes);
        match result {
            Ok(_) => {
                // Store these bytes
                header_data.extend_from_slice(&object_num_bytes);
                // Convert the object number bytes to an u32 integer
                let object_num = u32::from_be_bytes(object_num_bytes);
                // Return the number of objects and the header data for further processing
                Ok((object_num, header_data))
            },
            Err(_e) => {
                // If there is an error in reading, return a GitError
                Err(GitError::InvalidPackHeader(format!(
                    "{},{},{},{}",
                    object_num_bytes[0], object_num_bytes[1], object_num_bytes[2], object_num_bytes[3]
                )))
            }
        }
    }

    /// Decompresses data from a given Read and BufRead source using Zlib decompression.
    ///
    /// # Parameters
    /// * `pack`: A source that implements both Read and BufRead traits (e.g., file, network stream).
    /// * `expected_size`: The expected decompressed size of the data.
    ///
    /// # Returns
    /// Returns a `Result` containing either:
    /// * A tuple with a `Vec<u8>` of decompressed data, a `Vec<u8>` of the original compressed data,
    ///   and the total number of input bytes processed,
    /// * Or a `GitError` in case of a mismatch in expected size or any other reading error.
    ///
    pub fn decompress_data(&mut self, pack: &mut (impl Read + BufRead + Send), expected_size: usize, ) -> Result<(Vec<u8>, usize), GitError> {
        // Create a buffer with the expected size for the decompressed data
        let mut buf = Vec::with_capacity(expected_size);
        // Create a new Zlib decoder with the original data
        let mut deflate = ZlibDecoder::new(pack);

        // Attempt to read data to the end of the buffer
        match deflate.read_to_end(&mut buf) {
            Ok(_) => {
                // Check if the length of the buffer matches the expected size
                if buf.len() != expected_size {
                    Err(GitError::InvalidPackFile(format!(
                        "The object size {} does not match the expected size {}",
                        buf.len(),
                        expected_size
                    )))
                } else {
                    // If everything is as expected, return the buffer, the original data, and the total number of input bytes processed
                    Ok((buf, deflate.total_in() as usize))
                    // TODO this will likely be smaller than what the decompressor actually read from the underlying stream due to buffering.
                }
            },
            Err(e) => {
                // If there is an error in reading, return a GitError
                Err(GitError::InvalidPackFile(format!( "Decompression error: {}", e)))
            }
        }
    }

    /// Decodes a pack object from a given Read and BufRead source and returns the original compressed data.
    ///
    /// # Parameters
    /// * `pack`: A source that implements both Read and BufRead traits.
    /// * `offset`: A mutable reference to the current offset within the pack.
    ///
    /// # Returns
    /// Returns a `Result` containing either:
    /// * A tuple of the next offset in the pack and the original compressed data as `Vec<u8>`,
    /// * Or a `GitError` in case of any reading or decompression error.
    ///
    pub fn decode_pack_object(&mut self, pack: &mut (impl Read + BufRead + Send), offset: &mut usize) -> Result<CacheObject, GitError> {
        let init_offset = *offset;

        // Attempt to read the type and size, handle potential errors
        let (type_bits, size) = match utils::read_type_and_varint_size(pack, offset) {
            Ok(result) => result,
            Err(e) => {
                // Handle the error e.g., by logging it or converting it to GitError
                // and then return from the function
                return Err(GitError::InvalidPackFile(format!("Read error: {}", e)));
            }
        };

        // Check if the object type is valid
        let t = ObjectType::from_u8(type_bits)?;

        // util lambda: return data with result capacity after rebuilding, for Memory Control
        let reserve_delta_data = |data: Vec<u8>| -> Vec<u8> {
            let result_size = { // Read `result-size` of delta_obj
                let mut reader = Cursor::new(&data);
                let _ = utils::read_varint_le(&mut reader).unwrap().0; // base_size
                utils::read_varint_le(&mut reader).unwrap().0 // size after rebuilding
            };
            // capacity() == result_size, len() == data.len()
            // just for accurate Memory Control (rely on `heap_size()` that based on capacity)
            // Seems wasteful temporarily, but for final memory limit.
            let mut data_result_cap = Vec::with_capacity(result_size as usize);
            data_result_cap.extend(data);
            data_result_cap
        };

        match t {
            ObjectType::Commit | ObjectType::Tree | ObjectType::Blob | ObjectType::Tag => {
                let (data, raw_size) = self.decompress_data(pack, size)?;
                *offset += raw_size;
                Ok(CacheObject::new_for_undeltified(t, data, init_offset))
            },
            ObjectType::OffsetDelta => {
                let (delta_offset, bytes) = utils::read_offset_encoding(pack).unwrap();
                *offset += bytes;

                let (data, raw_size) = self.decompress_data(pack, size)?;
                *offset += raw_size;

                // Count the base object offset: the current offset - delta offset
                let base_offset = init_offset
                    .checked_sub(delta_offset as usize)
                    .ok_or_else(|| {
                        GitError::InvalidObjectInfo("Invalid OffsetDelta offset".to_string())
                    })
                    .unwrap();

                Ok(CacheObject {
                    base_offset,
                    data_decompress: reserve_delta_data(data),
                    obj_type: t,
                    offset: init_offset,
                    mem_recorder: None,
                    ..Default::default()
                })
            },
            ObjectType::HashDelta => {
                // Read 20 bytes to get the reference object SHA1 hash
                let mut buf_ref = [0; 20];
                pack.read_exact(&mut buf_ref).unwrap();
                let ref_sha1 = SHA1::from_bytes(buf_ref.as_ref()); //TODO SHA1::from_stream()
                // Offset is incremented by 20 bytes
                *offset += 20; //TODO 改为常量

                let (data, raw_size) = self.decompress_data(pack, size)?;
                *offset += raw_size;

                Ok(CacheObject {
                    base_ref: ref_sha1,
                    data_decompress: reserve_delta_data(data),
                    obj_type: t,
                    offset: init_offset,
                    mem_recorder: None,
                    ..Default::default()
                })
            }
        }
    }

    /// Decodes a pack file from a given Read and BufRead source and get a vec of objects.
    ///
    ///
    pub fn decode<F>(&mut self, pack: &mut (impl Read + BufRead + Seek + Send), callback: F) -> Result<(), GitError>
    where
        F: Fn(Entry) + Sync + Send + 'static
    {
        let time = Instant::now();
        let callback = Arc::new(callback);

        let caches = self.caches.clone();
        let mut reader = Wrapper::new(io::BufReader::new(pack));

        let result = Pack::check_header(&mut reader);
        match result {
            Ok((object_num, _)) => {
                self.number = object_num as usize;
            },
            Err(e) => {
                return Err(e);
            }
        }
        println!("The pack file has {} objects", self.number);

        let mut offset: usize = 12;
        let i = Arc::new(AtomicUsize::new(1));
        
        // debug log thread g   
        #[cfg(debug_assertions)]
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(debug_assertions)]
        { // LOG
            let log_pool = self.pool.clone();
            let log_cache = caches.clone();
            let log_i = i.clone();
            let log_stop =  stop.clone();
            let cache_objs_mem = self.cache_objs_mem.clone();
            // print log per seconds
            thread::spawn(move|| {
                let time = Instant::now();
                loop {
                    if log_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    println!("time {:?} s \t pass: {:?}, \t dec-num: {} \t cah-num: {} \t Objs: {} MB \t CacheUsed: {} MB",
                    time.elapsed().as_millis() as f64 / 1000.0, log_i.load(Ordering::Relaxed), log_pool.queued_count(), log_cache.queued_tasks(),
                             cache_objs_mem.load(Ordering::Relaxed) / 1024 / 1024,
                             log_cache.memory_used() / 1024 / 1024);

                    sleep(std::time::Duration::from_secs(1));
                }
            });
        } // LOG

        while i.load(Ordering::Relaxed) <= self.number {
            // 3 parts: Waitlist + TheadPool + Caches
            // hardcode the limit of the tasks of threads_pool queue, to limit memory
            while self.memory_used() > self.mem_limit || self.pool.queued_count() > 2000 {
                thread::yield_now();
            }
            let r: Result<CacheObject, GitError> = self.decode_pack_object(&mut reader, &mut offset);
            match r {
                Ok(mut obj) => {
                    obj.set_mem_recorder(self.cache_objs_mem.clone());
                    obj.record_mem_size();

                    // Wrapper of Arc Params, for convenience to pass
                    let params = Arc::new(SharedParams {
                        pool: self.pool.clone(),
                        waitlist: self.waitlist.clone(),
                        caches: self.caches.clone(),
                        cache_objs_mem_size: self.cache_objs_mem.clone(),
                        callback: callback.clone()
                    });

                    let caches = caches.clone();
                    let waitlist = self.waitlist.clone();
                    self.pool.execute(move || {
                        match obj.obj_type {
                            ObjectType::Commit | ObjectType::Tree | ObjectType::Blob | ObjectType::Tag => {
                                Self::cache_obj_and_process_waitlist(params, obj);
                            },
                            ObjectType::OffsetDelta => {
                                if let Some(base_obj) = caches.get_by_offset(obj.base_offset) {
                                    Self::process_delta(params, obj, base_obj);
                                } else {
                                    // You can delete this 'if' block ↑, because there are Second check in 'else'
                                    // It will be more readable, but the performance will be slightly reduced
                                    let base_offset = obj.base_offset;
                                    waitlist.insert_offset(obj.base_offset, obj);
                                    // Second check: prevent that the base_obj thread has finished before the waitlist insert
                                    if let Some(base_obj) = caches.get_by_offset(base_offset) {
                                        Self::process_waitlist(params, base_obj);
                                    }
                                }
                            },
                            ObjectType::HashDelta => {
                                if let Some(base_obj) = caches.get_by_hash(obj.base_ref) {
                                    Self::process_delta(params, obj, base_obj);
                                } else {
                                    let base_ref = obj.base_ref;
                                    waitlist.insert_ref(obj.base_ref, obj);
                                    if let Some(base_obj) = caches.get_by_hash(base_ref) {
                                        Self::process_waitlist(params, base_obj);
                                    }
                                }
                            }
                        }
                    });
                },
                Err(e) => {
                    return Err(e);
                }
            }
            i.fetch_add(1, Ordering::Relaxed);
        }

        let render_hash = reader.final_hash();
        let mut trailer_buf = [0; 20];
        reader.read_exact(&mut trailer_buf).unwrap();
        self.signature = SHA1::from_bytes(trailer_buf.as_ref());

        if render_hash != self.signature {
            return Err(GitError::InvalidPackFile(format!(
                "The pack file hash {} does not match the trailer hash {}",
                render_hash.to_plain_str(),
                self.signature.to_plain_str()
            )));
        }

        let end = utils::is_eof(&mut reader);
        if !end {
            return Err(GitError::InvalidPackFile(
                "The pack file is not at the end".to_string()
            ));
        }

        self.pool.join(); // wait for all threads to finish
        // !Attention: Caches threadpool may not stop, but it's not a problem (garbage file data)
        // So that files != self.number
        assert_eq!(self.waitlist.map_offset.len(), 0);
        assert_eq!(self.waitlist.map_ref.len(), 0);
        assert_eq!(self.number, caches.total_inserted());
        println!("The pack file has been decoded successfully");
        println!("Pack decode takes: [ {:?} ]", time.elapsed());

        self.caches.clear(); // clear cached objects & stop threads
        assert_eq!(self.cache_objs_mem_used(), 0); // all the objs should be dropped until here
        
        #[cfg(debug_assertions)]
        stop.store(true, Ordering::Relaxed);
        
        Ok(())
    }

    /// Decode Pack in a new thread and send the CacheObjects while decoding.
    /// <br> Attention: It will consume the `pack` and return in JoinHandle
    pub fn decode_async(mut self, mut pack: (impl Read + BufRead + Seek + Send + 'static), sender: Sender<Entry>) -> JoinHandle<Pack> {
        thread::spawn(move || {
            self.decode(&mut pack, move |entry| {
                sender.send(entry).unwrap();
            }).unwrap();
            self
        })
    }

    /// CacheObjects + Index size of Caches
    fn memory_used(&self) -> usize {
        self.cache_objs_mem_used() + self.caches.memory_used_index()
    }

    /// The total memory used by the CacheObjects of this Pack
    fn cache_objs_mem_used(&self) -> usize {
        self.cache_objs_mem.load(Ordering::Relaxed)
    }

    /// Rebuild the Delta Object in a new thread & process the objects waiting for it recursively.
    /// <br> This function must be *static*, because [&self] can't be moved into a new thread.
    fn process_delta(shared_params: Arc<SharedParams>, delta_obj: CacheObject, base_obj: Arc<CacheObject>) {
        shared_params.pool.clone().execute(move || {
            let mut new_obj = Pack::rebuild_delta(delta_obj, base_obj);
            new_obj.set_mem_recorder(shared_params.cache_objs_mem_size.clone());
            new_obj.record_mem_size();
            Self::cache_obj_and_process_waitlist(shared_params, new_obj); //Indirect Recursion
        });
    }

    /// Cache the new object & process the objects waiting for it (in multi-threading).
    fn cache_obj_and_process_waitlist(shared_params: Arc<SharedParams>, new_obj: CacheObject) {
        (shared_params.callback)(new_obj.to_entry());
        let new_obj = shared_params.caches.insert(new_obj.offset, new_obj.hash, new_obj);
        Self::process_waitlist(shared_params, new_obj);
    }

    fn process_waitlist(shared_params: Arc<SharedParams>, base_obj: Arc<CacheObject>) {
        let wait_objs = shared_params.waitlist.take(base_obj.offset, base_obj.hash);
        for obj in wait_objs {
            // Process the objects waiting for the new object(base_obj = new_obj)
            Self::process_delta(shared_params.clone(), obj, base_obj.clone());
        }
    }

    /// Reconstruct the Delta Object based on the "base object"
    /// and return a New object.
    pub fn rebuild_delta(delta_obj: CacheObject, base_obj: Arc<CacheObject>) -> CacheObject {
        const COPY_INSTRUCTION_FLAG: u8 = 1 << 7;
        const COPY_OFFSET_BYTES: u8 = 4;
        const COPY_SIZE_BYTES: u8 = 3;
        const COPY_ZERO_SIZE: usize = 0x10000;

        let mut stream = Cursor::new(&delta_obj.data_decompress);

        // Read the base object size & Result Size
        // (Size Encoding)
        let base_size = utils::read_varint_le(&mut stream).unwrap().0;
        let result_size = utils::read_varint_le(&mut stream).unwrap().0;

        //Get the base object row data
        let base_info = &base_obj.data_decompress;
        assert_eq!(base_info.len() as u64, base_size);

        let mut result = Vec::with_capacity(result_size as usize);

        loop {
            // Check if the stream has ended, meaning the new object is done
            let instruction = match utils::read_bytes(&mut stream) {
                Ok([instruction]) => instruction,
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                Err(err) => {
                    panic!(
                        "{}",
                        GitError::DeltaObjectError(format!("Wrong instruction in delta :{}", err))
                    );
                }
            };

            if instruction & COPY_INSTRUCTION_FLAG == 0 {
                // Data instruction; the instruction byte specifies the number of data bytes
                if instruction == 0 {
                    // Appending 0 bytes doesn't make sense, so git disallows it
                    panic!(
                        "{}",
                        GitError::DeltaObjectError(String::from("Invalid data instruction"))
                    );
                }

                // Append the provided bytes
                let mut data = vec![0; instruction as usize];
                stream.read_exact(&mut data).unwrap();
                result.extend_from_slice(&data);
            } else {
                // Copy instruction
                // +----------+---------+---------+---------+---------+-------+-------+-------+
                // | 1xxxxxxx | offset1 | offset2 | offset3 | offset4 | size1 | size2 | size3 |
                // +----------+---------+---------+---------+---------+-------+-------+-------+
                let mut nonzero_bytes = instruction;
                let offset = utils::read_partial_int(&mut stream, COPY_OFFSET_BYTES, &mut nonzero_bytes).unwrap();
                let mut size = utils::read_partial_int(&mut stream, COPY_SIZE_BYTES, &mut nonzero_bytes).unwrap();
                if size == 0 {
                    // Copying 0 bytes doesn't make sense, so git assumes a different size
                    size = COPY_ZERO_SIZE;
                }
                // Copy bytes from the base object
                let base_data = base_info.get(offset..(offset + size)).ok_or_else(|| {
                    GitError::DeltaObjectError("Invalid copy instruction".to_string())
                });

                match base_data {
                    Ok(data) => result.extend_from_slice(data),
                    Err(e) => panic!("{}", e),
                }
            }
        }
        assert_eq!(result_size, result.len() as u64);

        let hash = utils::calculate_object_hash(base_obj.obj_type, &result);
        // create new obj from `delta_obj` & `result` instead of modifying `delta_obj` for heap-size recording
        CacheObject {
            data_decompress: result,
            obj_type: base_obj.obj_type, // Same as the Type of base object
            hash,
            mem_recorder: None, // This filed(Arc) can't be moved from `delta_obj` by `struct update syntax`
            ..delta_obj // This syntax is actually move `delta_obj` to `new_obj`
        } // Canonical form (Complete Object)
        // mem_size recorder will be set later outside, to keep this func param clear
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::prelude::*;
    use std::io::BufReader;
    use std::io::Cursor;
    use std::{env, path::PathBuf};

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use crate::internal::pack::Pack;

    #[test]
    fn test_pack_check_header() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/git-2d187177923cd618a75da6c6db45bb89d92bd504.pack");

        let f = std::fs::File::open(source).unwrap();
        let mut buf_reader = BufReader::new(f);
        let (object_num, _) = Pack::check_header(&mut buf_reader).unwrap();

        assert_eq!(object_num, 358109);
    }

    #[test]
    fn test_decompress_data() {
        let data = b"Hello, world!"; // Sample data to compress and then decompress
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed_data = encoder.finish().unwrap();
        let compressed_size = compressed_data.len();

        // Create a cursor for the compressed data to simulate a Read + BufRead source
        let mut cursor: Cursor<Vec<u8>> = Cursor::new(compressed_data);
        let expected_size = data.len();

        // Decompress the data and assert correctness
        let mut p = Pack::new(None, None, None);
        let result = p.decompress_data(&mut cursor, expected_size);
        match result {
            Ok((decompressed_data, bytes_read)) => {
                assert_eq!(bytes_read, compressed_size);
                assert_eq!(decompressed_data, data);
            },
            Err(e) => panic!("Decompression failed: {:?}", e),
        }
    }

    #[test]
    fn test_pack_decode_without_delta() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/pack-1d0e6c14760c956c173ede71cb28f33d921e232f.pack");

        let tmp = PathBuf::from("/tmp/.cache_temp");

        let f = std::fs::File::open(source).unwrap();
        let mut buffered = BufReader::new(f);
        let mut p = Pack::new(None, Some(1024*1024*20), Some(tmp));
        p.decode(&mut buffered, |_|{}).unwrap();
    }

    #[test]
    fn test_pack_decode_with_ref_delta() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/ref-delta-65d47638aa7cb7c39f1bd1d5011a415439b887a8.pack");

        let tmp = PathBuf::from("/tmp/.cache_temp");

        let f = std::fs::File::open(source).unwrap();
        let mut buffered = BufReader::new(f);
        let mut p = Pack::new(None, Some(1024*1024*20), Some(tmp));
        p.decode(&mut buffered,|_|{}).unwrap();
    }

    #[test]
    fn test_pack_decode_with_large_file_with_delta_without_ref() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/git-2d187177923cd618a75da6c6db45bb89d92bd504.pack");

        let tmp = PathBuf::from("/tmp/.cache_temp");

        let f = std::fs::File::open(source).unwrap();
        let mut buffered = BufReader::new(f);
        // let mut p = Pack::default(); //Pack::new(2);
        let mut p = Pack::new(Some(20), Some(1024*1024*1024*2), Some(tmp.clone()));
        let rt = p.decode(&mut buffered, |_obj|{
            // println!("{:?}", obj.hash);
        });
        if let Err(e) = rt {
            fs::remove_dir_all(tmp).unwrap();
            panic!("Error: {:?}", e);
        }
    } // it will be stuck on dropping `Pack` on Windows if `mem_size` is None, so we need `mimalloc`

    #[test]
    fn test_decode_large_file_async() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/git-2d187177923cd618a75da6c6db45bb89d92bd504.pack");

        let tmp = PathBuf::from("/tmp/.cache_temp");
        let f = fs::File::open(source).unwrap();
        let buffered = BufReader::new(f);
        let p = Pack::new(Some(20), Some(1024*1024*1024*2), Some(tmp.clone()));

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = p.decode_async(buffered, tx); // new thread
        let mut cnt = 0;
        for _entry in rx {
            cnt += 1; //use entry here
        }
        let p = handle.join().unwrap();
        assert_eq!(cnt, p.number);
    }

    #[test]
    fn test_pack_decode_with_delta_without_ref() {
        let mut source = PathBuf::from(env::current_dir().unwrap().parent().unwrap());
        source.push("tests/data/packs/pack-d50df695086eea6253a237cb5ac44af1629e7ced.pack");

        let tmp = PathBuf::from("/tmp/.cache_temp");

        let f = std::fs::File::open(source).unwrap();
        let mut buffered = BufReader::new(f);
        let mut p = Pack::new(None, Some(1024*1024*20), Some(tmp));
        p.decode(&mut buffered, |_|{}).unwrap();
    }

    #[test]
    fn test_pack_decode_multi_task_with_large_file_with_delta_without_ref() {
        let task1 = std::thread::spawn(|| {
            test_pack_decode_with_large_file_with_delta_without_ref();
        });
        let task2 = std::thread::spawn(|| {
            test_pack_decode_with_large_file_with_delta_without_ref();
        });

        task1.join().unwrap();
        task2.join().unwrap();
    }
}
