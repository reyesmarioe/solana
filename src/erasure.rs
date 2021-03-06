// Support erasure coding
use crate::db_ledger::DbLedger;
use crate::db_window::{find_missing_coding_indexes, find_missing_data_indexes};
use crate::packet::{Blob, SharedBlob, BLOB_DATA_SIZE, BLOB_HEADER_SIZE, BLOB_SIZE};
use crate::result::{Error, Result};
use crate::window::WindowSlot;
use solana_sdk::pubkey::Pubkey;
use std::cmp;
use std::sync::{Arc, RwLock};

//TODO(sakridge) pick these values
pub const NUM_DATA: usize = 16; // number of data blobs
pub const NUM_CODING: usize = 4; // number of coding blobs, also the maximum number that can go missing
pub const ERASURE_SET_SIZE: usize = NUM_DATA + NUM_CODING; // total number of blobs in an erasure set, includes data and coding blobs

pub const JERASURE_ALIGN: usize = 4; // data size has to be a multiple of 4 bytes

macro_rules! align {
    ($x:expr, $align:expr) => {
        $x + ($align - 1) & !($align - 1)
    };
}

#[derive(Debug, PartialEq, Eq)]
pub enum ErasureError {
    NotEnoughBlocksToDecode,
    DecodeError,
    EncodeError,
    InvalidBlockSize,
    InvalidBlobData,
}

// k = number of data devices
// m = number of coding devices
// w = word size

extern "C" {
    fn jerasure_matrix_encode(
        k: i32,
        m: i32,
        w: i32,
        matrix: *const i32,
        data_ptrs: *const *const u8,
        coding_ptrs: *const *mut u8,
        size: i32,
    );
    fn jerasure_matrix_decode(
        k: i32,
        m: i32,
        w: i32,
        matrix: *const i32,
        row_k_ones: i32,
        erasures: *const i32,
        data_ptrs: *const *mut u8,
        coding_ptrs: *const *mut u8,
        size: i32,
    ) -> i32;
    fn galois_single_divide(a: i32, b: i32, w: i32) -> i32;
}

fn get_matrix(m: i32, k: i32, w: i32) -> Vec<i32> {
    let mut matrix = vec![0; (m * k) as usize];
    for i in 0..m {
        for j in 0..k {
            unsafe {
                matrix[(i * k + j) as usize] = galois_single_divide(1, i ^ (m + j), w);
            }
        }
    }
    matrix
}

pub const ERASURE_W: i32 = 32;

// Generate coding blocks into coding
//   There are some alignment restrictions, blocks should be aligned by 16 bytes
//   which means their size should be >= 16 bytes
pub fn generate_coding_blocks(coding: &mut [&mut [u8]], data: &[&[u8]]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let k = data.len() as i32;
    let m = coding.len() as i32;
    let block_len = data[0].len() as i32;
    let matrix: Vec<i32> = get_matrix(m, k, ERASURE_W);
    let mut data_arg = Vec::with_capacity(data.len());
    for block in data {
        if block_len != block.len() as i32 {
            error!(
                "data block size incorrect {} expected {}",
                block.len(),
                block_len
            );
            return Err(Error::ErasureError(ErasureError::InvalidBlockSize));
        }
        data_arg.push(block.as_ptr());
    }
    let mut coding_arg = Vec::with_capacity(coding.len());
    for block in coding {
        if block_len != block.len() as i32 {
            error!(
                "coding block size incorrect {} expected {}",
                block.len(),
                block_len
            );
            return Err(Error::ErasureError(ErasureError::InvalidBlockSize));
        }
        coding_arg.push(block.as_mut_ptr());
    }

    unsafe {
        jerasure_matrix_encode(
            k,
            m,
            ERASURE_W,
            matrix.as_ptr(),
            data_arg.as_ptr(),
            coding_arg.as_ptr(),
            block_len,
        );
    }
    Ok(())
}

// Recover data + coding blocks into data blocks
//   data: array of blocks to recover into
//   coding: arry of coding blocks
//   erasures: list of indices in data where blocks should be recovered
pub fn decode_blocks(
    data: &mut [&mut [u8]],
    coding: &mut [&mut [u8]],
    erasures: &[i32],
) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let block_len = data[0].len();
    let matrix: Vec<i32> = get_matrix(coding.len() as i32, data.len() as i32, ERASURE_W);

    // generate coding pointers, blocks should be the same size
    let mut coding_arg: Vec<*mut u8> = Vec::new();
    for x in coding.iter_mut() {
        if x.len() != block_len {
            return Err(Error::ErasureError(ErasureError::InvalidBlockSize));
        }
        coding_arg.push(x.as_mut_ptr());
    }

    // generate data pointers, blocks should be the same size
    let mut data_arg: Vec<*mut u8> = Vec::new();
    for x in data.iter_mut() {
        if x.len() != block_len {
            return Err(Error::ErasureError(ErasureError::InvalidBlockSize));
        }
        data_arg.push(x.as_mut_ptr());
    }
    let ret = unsafe {
        jerasure_matrix_decode(
            data.len() as i32,
            coding.len() as i32,
            ERASURE_W,
            matrix.as_ptr(),
            0,
            erasures.as_ptr(),
            data_arg.as_ptr(),
            coding_arg.as_ptr(),
            data[0].len() as i32,
        )
    };
    trace!("jerasure_matrix_decode ret: {}", ret);
    for x in data[erasures[0] as usize][0..8].iter() {
        trace!("{} ", x)
    }
    trace!("");
    if ret < 0 {
        return Err(Error::ErasureError(ErasureError::DecodeError));
    }
    Ok(())
}

// Generate coding blocks in window starting from start_idx,
//   for num_blobs..  For each block place the coding blobs
//   at the end of the block like so:
//
//  block-size part of a Window, with each element a WindowSlot..
//  |<======================= NUM_DATA ==============================>|
//                                              |<==== NUM_CODING ===>|
//  +---+ +---+ +---+ +---+ +---+         +---+ +---+ +---+ +---+ +---+
//  | D | | D | | D | | D | | D |         | D | | D | | D | | D | | D |
//  +---+ +---+ +---+ +---+ +---+  . . .  +---+ +---+ +---+ +---+ +---+
//  |   | |   | |   | |   | |   |         |   | | C | | C | | C | | C |
//  +---+ +---+ +---+ +---+ +---+         +---+ +---+ +---+ +---+ +---+
//
//  blob structure for coding, recover
//
//   + ------- meta is set and used by transport, meta.size is actual length
//   |           of data in the byte array blob.data
//   |
//   |          + -- data is stuff shipped over the wire, and has an included
//   |          |        header
//   V          V
//  +----------+------------------------------------------------------------+
//  | meta     |  data                                                      |
//  |+---+--   |+---+---+---+---+------------------------------------------+|
//  || s | .   || i |   | f | s |                                          ||
//  || i | .   || n | i | l | i |                                          ||
//  || z | .   || d | d | a | z |     blob.data(), or blob.data_mut()      ||
//  || e |     || e |   | g | e |                                          ||
//  |+---+--   || x |   | s |   |                                          ||
//  |          |+---+---+---+---+------------------------------------------+|
//  +----------+------------------------------------------------------------+
//             |                |<=== coding blob part for "coding" =======>|
//             |                                                            |
//             |<============== data blob part for "coding"  ==============>|
//
//
//
pub fn generate_coding(
    id: &Pubkey,
    window: &mut [WindowSlot],
    receive_index: u64,
    num_blobs: usize,
    transmit_index_coding: &mut u64,
) -> Result<()> {
    // beginning of the coding blobs of the block that receive_index points into
    let coding_index_start =
        receive_index - (receive_index % NUM_DATA as u64) + (NUM_DATA - NUM_CODING) as u64;

    let start_idx = receive_index as usize % window.len();
    let mut block_start = start_idx - (start_idx % NUM_DATA);

    loop {
        let block_end = block_start + NUM_DATA;
        if block_end > (start_idx + num_blobs) {
            break;
        }
        info!(
            "generate_coding {} start: {} end: {} start_idx: {} num_blobs: {}",
            id, block_start, block_end, start_idx, num_blobs
        );

        let mut max_data_size = 0;

        // find max_data_size, maybe bail if not all the data is here
        for i in block_start..block_end {
            let n = i % window.len();
            trace!("{} window[{}] = {:?}", id, n, window[n].data);

            if let Some(b) = &window[n].data {
                max_data_size = cmp::max(b.read().unwrap().meta.size, max_data_size);
            } else {
                trace!("{} data block is null @ {}", id, n);
                return Ok(());
            }
        }

        // round up to the nearest jerasure alignment
        max_data_size = align!(max_data_size, JERASURE_ALIGN);

        let mut data_blobs = Vec::with_capacity(NUM_DATA);
        for i in block_start..block_end {
            let n = i % window.len();

            if let Some(b) = &window[n].data {
                // make sure extra bytes in each blob are zero-d out for generation of
                //  coding blobs
                let mut b_wl = b.write().unwrap();
                for i in b_wl.meta.size..max_data_size {
                    b_wl.data[i] = 0;
                }
                data_blobs.push(b);
            }
        }

        // getting ready to do erasure coding, means that we're potentially
        // going back in time, tell our caller we've inserted coding blocks
        // starting at coding_index_start
        *transmit_index_coding = cmp::min(*transmit_index_coding, coding_index_start);

        let mut coding_blobs = Vec::with_capacity(NUM_CODING);
        let coding_start = block_end - NUM_CODING;
        for i in coding_start..block_end {
            let n = i % window.len();
            assert!(window[n].coding.is_none());

            window[n].coding = Some(SharedBlob::default());

            let coding = window[n].coding.clone().unwrap();
            let mut coding_wl = coding.write().unwrap();
            for i in 0..max_data_size {
                coding_wl.data[i] = 0;
            }
            // copy index and id from the data blob
            if let Some(data) = &window[n].data {
                let data_rl = data.read().unwrap();

                let index = data_rl.index().unwrap();
                let slot = data_rl.slot().unwrap();
                let id = data_rl.id().unwrap();

                trace!(
                    "{} copying index {} id {:?} from data to coding",
                    id,
                    index,
                    id
                );
                coding_wl.set_index(index).unwrap();
                coding_wl.set_slot(slot).unwrap();
                coding_wl.set_id(&id).unwrap();
            }
            coding_wl.set_size(max_data_size);
            if coding_wl.set_coding().is_err() {
                return Err(Error::ErasureError(ErasureError::EncodeError));
            }

            coding_blobs.push(coding.clone());
        }

        let data_locks: Vec<_> = data_blobs.iter().map(|b| b.read().unwrap()).collect();

        let data_ptrs: Vec<_> = data_locks
            .iter()
            .enumerate()
            .map(|(i, l)| {
                trace!("{} i: {} data: {}", id, i, l.data[0]);
                &l.data[..max_data_size]
            })
            .collect();

        let mut coding_locks: Vec<_> = coding_blobs.iter().map(|b| b.write().unwrap()).collect();

        let mut coding_ptrs: Vec<_> = coding_locks
            .iter_mut()
            .enumerate()
            .map(|(i, l)| {
                trace!("{} i: {} coding: {}", id, i, l.data[0],);
                &mut l.data_mut()[..max_data_size]
            })
            .collect();

        generate_coding_blocks(coding_ptrs.as_mut_slice(), &data_ptrs)?;
        debug!(
            "{} start_idx: {} data: {}:{} coding: {}:{}",
            id, start_idx, block_start, block_end, coding_start, block_end
        );
        block_start = block_end;
    }
    Ok(())
}

// Recover the missing data and coding blobs from the input ledger. Returns a vector
// of the recovered missing data blobs and a vector of the recovered coding blobs
pub fn recover(
    db_ledger: &Arc<DbLedger>,
    slot: u64,
    start_idx: u64,
) -> Result<(Vec<SharedBlob>, Vec<SharedBlob>)> {
    let block_start_idx = start_idx - (start_idx % NUM_DATA as u64);

    debug!("block_start_idx: {}", block_start_idx);

    let coding_start_idx = block_start_idx + NUM_DATA as u64 - NUM_CODING as u64;
    let block_end_idx = block_start_idx + NUM_DATA as u64;
    trace!(
        "recover: coding_start_idx: {} block_end_idx: {}",
        coding_start_idx,
        block_end_idx
    );

    let data_missing =
        find_missing_data_indexes(slot, &db_ledger, block_start_idx, block_end_idx, NUM_DATA).len();
    let coding_missing = find_missing_coding_indexes(
        slot,
        &db_ledger,
        coding_start_idx,
        block_end_idx,
        NUM_CODING,
    )
    .len();

    // if we're not missing data, or if we have too much missing but have enough coding
    if data_missing == 0 {
        // nothing to do...
        return Ok((vec![], vec![]));
    }

    if (data_missing + coding_missing) > NUM_CODING {
        trace!(
            "recover: start: {} skipping recovery data: {} coding: {}",
            block_start_idx,
            data_missing,
            coding_missing
        );
        // nothing to do...
        return Err(Error::ErasureError(ErasureError::NotEnoughBlocksToDecode));
    }

    trace!(
        "recover: recovering: data: {} coding: {}",
        data_missing,
        coding_missing
    );

    let mut blobs: Vec<SharedBlob> = Vec::with_capacity(NUM_DATA + NUM_CODING);
    let mut erasures: Vec<i32> = Vec::with_capacity(NUM_CODING);

    let mut missing_data: Vec<SharedBlob> = vec![];
    let mut missing_coding: Vec<SharedBlob> = vec![];
    let mut size = None;

    // Add the data blobs we have into the recovery vector, mark the missing ones
    for i in block_start_idx..block_end_idx {
        let result = db_ledger.data_cf.get_by_slot_index(slot, i)?;

        categorize_blob(
            &result,
            &mut blobs,
            &mut missing_data,
            &mut erasures,
            (i - block_start_idx) as i32,
        )?;
    }

    // Add the coding blobs we have into the recovery vector, mark the missing ones
    for i in coding_start_idx..block_end_idx {
        let result = db_ledger.erasure_cf.get_by_slot_index(slot, i)?;

        categorize_blob(
            &result,
            &mut blobs,
            &mut missing_coding,
            &mut erasures,
            ((i - coding_start_idx) + NUM_DATA as u64) as i32,
        )?;

        if let Some(b) = result {
            if size.is_none() {
                size = Some(b.len() - BLOB_HEADER_SIZE);
            }
        }
    }

    // Due to check (data_missing + coding_missing) > NUM_CODING from earlier in this function,
    // we know at least one coding block must exist, so "size" will not remain None after the
    // below processing.
    let size = size.unwrap();
    // marks end of erasures
    erasures.push(-1);
    trace!("erasures[]:{:?} data_size: {}", erasures, size,);

    let mut locks = Vec::with_capacity(NUM_DATA + NUM_CODING);
    {
        let mut coding_ptrs: Vec<&mut [u8]> = Vec::with_capacity(NUM_CODING);
        let mut data_ptrs: Vec<&mut [u8]> = Vec::with_capacity(NUM_DATA);

        for b in &blobs {
            locks.push(b.write().unwrap());
        }

        for (i, l) in locks.iter_mut().enumerate() {
            if i < NUM_DATA {
                data_ptrs.push(&mut l.data[..size]);
            } else {
                coding_ptrs.push(&mut l.data_mut()[..size]);
            }
        }

        // Decode the blocks
        decode_blocks(
            data_ptrs.as_mut_slice(),
            coding_ptrs.as_mut_slice(),
            &erasures,
        )?;
    }

    // Create the missing blobs from the reconstructed data
    let mut corrupt = false;

    for i in &erasures[..erasures.len() - 1] {
        let n = *i as usize;
        let mut idx = n as u64 + block_start_idx;

        let mut data_size;
        if n < NUM_DATA {
            data_size = locks[n].data_size().unwrap() as usize;
            data_size -= BLOB_HEADER_SIZE;
            if data_size > BLOB_DATA_SIZE {
                error!("corrupt data blob[{}] data_size: {}", idx, data_size);
                corrupt = true;
                break;
            }
        } else {
            data_size = size;
            idx -= NUM_CODING as u64;
            locks[n].set_slot(slot).unwrap();
            locks[n].set_index(idx).unwrap();

            if data_size - BLOB_HEADER_SIZE > BLOB_DATA_SIZE {
                error!("corrupt coding blob[{}] data_size: {}", idx, data_size);
                corrupt = true;
                break;
            }
        }

        locks[n].set_size(data_size);
        trace!(
            "erasures[{}] ({}) size: {} data[0]: {}",
            *i,
            idx,
            data_size,
            locks[n].data()[0]
        );
    }

    if corrupt {
        // Remove the corrupted coding blobs so there's no effort wasted in trying to reconstruct
        // the blobs again
        for i in coding_start_idx..block_end_idx {
            db_ledger.erasure_cf.delete_by_slot_index(slot, i)?;
        }
        return Ok((vec![], vec![]));
    }

    Ok((missing_data, missing_coding))
}

fn categorize_blob(
    get_blob_result: &Option<Vec<u8>>,
    blobs: &mut Vec<SharedBlob>,
    missing: &mut Vec<SharedBlob>,
    erasures: &mut Vec<i32>,
    erasure_index: i32,
) -> Result<()> {
    match get_blob_result {
        Some(b) => {
            if b.len() <= BLOB_HEADER_SIZE || b.len() > BLOB_SIZE {
                return Err(Error::ErasureError(ErasureError::InvalidBlobData));
            }
            blobs.push(Arc::new(RwLock::new(Blob::new(&b))));
        }
        None => {
            // Mark the missing memory
            erasures.push(erasure_index);
            let b = SharedBlob::default();
            blobs.push(b.clone());
            missing.push(b);
        }
    }

    Ok(())
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::db_ledger::{DbLedger, DEFAULT_SLOT_HEIGHT};
    use crate::ledger::{get_tmp_ledger_path, make_tiny_test_entries, Block};

    use crate::packet::{index_blobs, SharedBlob, BLOB_DATA_SIZE, BLOB_SIZE};
    use crate::window::WindowSlot;
    use rand::{thread_rng, Rng};
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, KeypairUtil};
    use std::sync::Arc;

    #[test]
    pub fn test_coding() {
        let zero_vec = vec![0; 16];
        let mut vs: Vec<Vec<u8>> = (0..4).map(|i| (i..(16 + i)).collect()).collect();
        let v_orig: Vec<u8> = vs[0].clone();

        let m = 2;
        let mut coding_blocks: Vec<_> = (0..m).map(|_| vec![0u8; 16]).collect();

        {
            let mut coding_blocks_slices: Vec<_> =
                coding_blocks.iter_mut().map(|x| x.as_mut_slice()).collect();
            let v_slices: Vec<_> = vs.iter().map(|x| x.as_slice()).collect();

            assert!(generate_coding_blocks(
                coding_blocks_slices.as_mut_slice(),
                v_slices.as_slice(),
            )
            .is_ok());
        }
        trace!("coding blocks:");
        for b in &coding_blocks {
            trace!("{:?}", b);
        }
        let erasure: i32 = 1;
        let erasures = vec![erasure, -1];
        // clear an entry
        vs[erasure as usize].copy_from_slice(zero_vec.as_slice());

        {
            let mut coding_blocks_slices: Vec<_> =
                coding_blocks.iter_mut().map(|x| x.as_mut_slice()).collect();
            let mut v_slices: Vec<_> = vs.iter_mut().map(|x| x.as_mut_slice()).collect();

            assert!(decode_blocks(
                v_slices.as_mut_slice(),
                coding_blocks_slices.as_mut_slice(),
                erasures.as_slice(),
            )
            .is_ok());
        }

        trace!("vs:");
        for v in &vs {
            trace!("{:?}", v);
        }
        assert_eq!(v_orig, vs[0]);
    }

    // TODO: Temprorary function used in tests to generate a database ledger
    // from the window (which is used to generate the erasure coding)
    // until we also transition generate_coding() and BroadcastStage to use DbLedger
    // Github issue: https://github.com/solana-labs/solana/issues/1899.
    pub fn generate_db_ledger_from_window(
        ledger_path: &str,
        window: &[WindowSlot],
        use_random: bool,
    ) -> DbLedger {
        let db_ledger =
            DbLedger::open(ledger_path).expect("Expected to be able to open database ledger");
        for slot in window {
            if let Some(ref data) = slot.data {
                // If we're using gibberish blobs, skip validation checks and insert
                // directly into the ledger
                if use_random {
                    let data_l = data.read().unwrap();
                    db_ledger
                        .data_cf
                        .put_by_slot_index(
                            data_l.slot().unwrap(),
                            data_l.index().unwrap(),
                            &data_l.data[..data_l.data_size().unwrap() as usize],
                        )
                        .expect("Expected successful put into data column of ledger");
                } else {
                    db_ledger
                        .write_shared_blobs(vec![data].into_iter())
                        .unwrap();
                }
            }

            if let Some(ref coding) = slot.coding {
                let coding_lock = coding.read().unwrap();

                let index = coding_lock
                    .index()
                    .expect("Expected coding blob to have valid index");

                let data_size = coding_lock
                    .size()
                    .expect("Expected coding blob to have valid data size");

                db_ledger
                    .erasure_cf
                    .put_by_slot_index(
                        coding_lock.slot().unwrap(),
                        index,
                        &coding_lock.data[..data_size as usize + BLOB_HEADER_SIZE],
                    )
                    .unwrap();
            }
        }

        db_ledger
    }

    pub fn setup_window_ledger(
        offset: usize,
        num_blobs: usize,
        use_random_window: bool,
        slot: u64,
    ) -> Vec<WindowSlot> {
        // Generate a window
        let mut window = {
            if use_random_window {
                generate_window(offset, num_blobs, slot)
            } else {
                generate_entry_window(offset, num_blobs)
            }
        };

        for slot in &window {
            if let Some(blob) = &slot.data {
                let blob_r = blob.read().unwrap();
                assert!(!blob_r.is_coding());
            }
        }

        // Generate the coding blocks
        let mut index = (NUM_DATA + 2) as u64;
        assert!(generate_coding(
            &Pubkey::default(),
            &mut window,
            offset as u64,
            num_blobs,
            &mut index
        )
        .is_ok());
        assert_eq!(index, (NUM_DATA - NUM_CODING) as u64);

        // put junk in the tails, simulates re-used blobs
        scramble_window_tails(&mut window, num_blobs);

        window
    }

    const WINDOW_SIZE: usize = 64;
    fn generate_window(offset: usize, num_blobs: usize, slot: u64) -> Vec<WindowSlot> {
        let mut window = vec![
            WindowSlot {
                data: None,
                coding: None,
                leader_unknown: false,
            };
            WINDOW_SIZE
        ];
        let mut blobs = Vec::with_capacity(num_blobs);
        for i in 0..num_blobs {
            let b = SharedBlob::default();
            let b_ = b.clone();
            let mut w = b.write().unwrap();
            // generate a random length, multiple of 4 between 8 and 32
            let data_len = if i == 3 {
                BLOB_DATA_SIZE
            } else {
                (thread_rng().gen_range(2, 8) * 4) + 1
            };

            eprintln!("data_len of {} is {}", i, data_len);
            w.set_size(data_len);

            for k in 0..data_len {
                w.data_mut()[k] = (k + i) as u8;
            }

            // overfill, simulates re-used blobs
            for i in BLOB_HEADER_SIZE + data_len..BLOB_SIZE {
                w.data[i] = thread_rng().gen();
            }

            blobs.push(b_);
        }

        {
            // Make some dummy slots
            let slot_tick_heights: Vec<(&SharedBlob, u64)> =
                blobs.iter().zip(vec![slot; blobs.len()]).collect();
            index_blobs(slot_tick_heights, &Keypair::new().pubkey(), offset as u64);
        }
        for b in blobs {
            let idx = b.read().unwrap().index().unwrap() as usize % WINDOW_SIZE;

            window[idx].data = Some(b);
        }
        window
    }

    fn generate_entry_window(offset: usize, num_blobs: usize) -> Vec<WindowSlot> {
        let mut window = vec![
            WindowSlot {
                data: None,
                coding: None,
                leader_unknown: false,
            };
            WINDOW_SIZE
        ];
        let entries = make_tiny_test_entries(num_blobs);
        let blobs = entries.to_shared_blobs();

        {
            // Make some dummy slots
            let slot_tick_heights: Vec<(&SharedBlob, u64)> = blobs
                .iter()
                .zip(vec![DEFAULT_SLOT_HEIGHT; blobs.len()])
                .collect();
            index_blobs(slot_tick_heights, &Keypair::new().pubkey(), offset as u64);
        }

        for b in blobs.into_iter() {
            let idx = b.read().unwrap().index().unwrap() as usize % WINDOW_SIZE;

            window[idx].data = Some(b);
        }
        window
    }

    fn scramble_window_tails(window: &mut [WindowSlot], num_blobs: usize) {
        for i in 0..num_blobs {
            if let Some(b) = &window[i].data {
                let size = {
                    let b_l = b.read().unwrap();
                    b_l.meta.size
                } as usize;

                let mut b_l = b.write().unwrap();
                for i in size..BLOB_SIZE {
                    b_l.data[i] = thread_rng().gen();
                }
            }
        }
    }

    // Remove a data block, test for successful recovery
    #[test]
    pub fn test_window_recover_basic() {
        solana_logger::setup();

        // Setup the window
        let offset = 0;
        let num_blobs = NUM_DATA + 2;
        let mut window = setup_window_ledger(offset, num_blobs, true, DEFAULT_SLOT_HEIGHT);

        println!("** whack data block:");
        // Test erasing a data block
        let erase_offset = offset % window.len();

        // Create a hole in the window
        let refwindow = window[erase_offset].data.clone();
        window[erase_offset].data = None;

        // Generate the db_ledger from the window
        let ledger_path = get_tmp_ledger_path("test_window_recover_basic");
        let db_ledger = Arc::new(generate_db_ledger_from_window(&ledger_path, &window, true));

        // Recover it from coding
        let (recovered_data, recovered_coding) = recover(&db_ledger, 0, offset as u64)
            .expect("Expected successful recovery of erased blobs");

        assert!(recovered_coding.is_empty());
        {
            // Check the result, block is here to drop locks
            let recovered_blob = recovered_data
                .first()
                .expect("Expected recovered data blob to exist");
            let ref_l = refwindow.clone().unwrap();
            let ref_l2 = ref_l.read().unwrap();
            let result = recovered_blob.read().unwrap();

            assert_eq!(result.size().unwrap(), ref_l2.size().unwrap());
            assert_eq!(
                result.data[..ref_l2.data_size().unwrap() as usize],
                ref_l2.data[..ref_l2.data_size().unwrap() as usize]
            );
            assert_eq!(result.index().unwrap(), offset as u64);
            assert_eq!(result.slot().unwrap(), DEFAULT_SLOT_HEIGHT as u64);
        }
        drop(db_ledger);
        DbLedger::destroy(&ledger_path)
            .expect("Expected successful destruction of database ledger");
    }

    // Remove a data and coding block, test for successful recovery
    #[test]
    pub fn test_window_recover_basic2() {
        solana_logger::setup();

        // Setup the window
        let offset = 0;
        let num_blobs = NUM_DATA + 2;
        let mut window = setup_window_ledger(offset, num_blobs, true, DEFAULT_SLOT_HEIGHT);

        println!("** whack coding block and data block");
        // Tests erasing a coding block and a data block
        let coding_start = offset - (offset % NUM_DATA) + (NUM_DATA - NUM_CODING);
        let erase_offset = coding_start % window.len();

        // Create a hole in the window
        let refwindowdata = window[erase_offset].data.clone();
        let refwindowcoding = window[erase_offset].coding.clone();
        window[erase_offset].data = None;
        window[erase_offset].coding = None;
        let ledger_path = get_tmp_ledger_path("test_window_recover_basic2");
        let db_ledger = Arc::new(generate_db_ledger_from_window(&ledger_path, &window, true));

        // Recover it from coding
        let (recovered_data, recovered_coding) = recover(&db_ledger, 0, offset as u64)
            .expect("Expected successful recovery of erased blobs");

        {
            let recovered_data_blob = recovered_data
                .first()
                .expect("Expected recovered data blob to exist");

            let recovered_coding_blob = recovered_coding
                .first()
                .expect("Expected recovered coding blob to exist");

            // Check the recovered data result
            let ref_l = refwindowdata.clone().unwrap();
            let ref_l2 = ref_l.read().unwrap();
            let result = recovered_data_blob.read().unwrap();

            assert_eq!(result.size().unwrap(), ref_l2.size().unwrap());
            assert_eq!(
                result.data[..ref_l2.data_size().unwrap() as usize],
                ref_l2.data[..ref_l2.data_size().unwrap() as usize]
            );
            assert_eq!(result.index().unwrap(), coding_start as u64);
            assert_eq!(result.slot().unwrap(), DEFAULT_SLOT_HEIGHT as u64);

            // Check the recovered erasure result
            let ref_l = refwindowcoding.clone().unwrap();
            let ref_l2 = ref_l.read().unwrap();
            let result = recovered_coding_blob.read().unwrap();

            assert_eq!(result.size().unwrap(), ref_l2.size().unwrap());
            assert_eq!(
                result.data()[..ref_l2.size().unwrap() as usize],
                ref_l2.data()[..ref_l2.size().unwrap() as usize]
            );
            assert_eq!(result.index().unwrap(), coding_start as u64);
            assert_eq!(result.slot().unwrap(), DEFAULT_SLOT_HEIGHT as u64);
        }
        drop(db_ledger);
        DbLedger::destroy(&ledger_path)
            .expect("Expected successful destruction of database ledger");
    }

    //    //TODO This needs to be reworked
    //    #[test]
    //    #[ignore]
    //    pub fn test_window_recover() {
    //        solana_logger::setup();
    //        let offset = 4;
    //        let data_len = 16;
    //        let num_blobs = NUM_DATA + 2;
    //        let (mut window, blobs_len) = generate_window(data_len, offset, num_blobs);
    //        println!("** after-gen:");
    //        print_window(&window);
    //        assert!(generate_coding(&mut window, offset, blobs_len).is_ok());
    //        println!("** after-coding:");
    //        print_window(&window);
    //        let refwindow = window[offset + 1].clone();
    //        window[offset + 1] = None;
    //        window[offset + 2] = None;
    //        window[offset + SET_SIZE + 3] = None;
    //        window[offset + (2 * SET_SIZE) + 0] = None;
    //        window[offset + (2 * SET_SIZE) + 1] = None;
    //        window[offset + (2 * SET_SIZE) + 2] = None;
    //        let window_l0 = &(window[offset + (3 * SET_SIZE)]).clone().unwrap();
    //        window_l0.write().unwrap().data[0] = 55;
    //        println!("** after-nulling:");
    //        print_window(&window);
    //        assert!(recover(&mut window, offset, offset + blobs_len).is_ok());
    //        println!("** after-restore:");
    //        print_window(&window);
    //        let window_l = window[offset + 1].clone().unwrap();
    //        let ref_l = refwindow.clone().unwrap();
    //        assert_eq!(
    //            window_l.read().unwrap().data()[..data_len],
    //            ref_l.read().unwrap().data()[..data_len]
    //        );
    //    }
}
