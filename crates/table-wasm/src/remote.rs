//! A file on the server: the blocks of it that are here, and what to ask for next.
//!
//! Blocks are kept in memory, or in a log on the disk: each with a head before
//! it, in the order in which they came. Which those are is found out by going
//! through the log.

use reticula_table::checksum::checksum;
use reticula_table::source::{Sink, Source, SourceError};

use crate::file::offset;

/// Bytes of a block. The last one has less. Of larger ones more than half of what
/// comes is not looked at, and smaller ones take too many requests.
pub const BLOCK: usize = 64 << 10;
/// Bytes of a head: which block it is of, and a sum of it.
const HEAD: usize = 8;
/// Requests that are under way at once, at most.
const MOST_REQUESTS: usize = 4;
/// Blocks of one request, at most.
const MOST_BLOCKS: u32 = 64;

/// What became of bytes that have come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    Kept,
    /// There is no room in the log, which is not used any more.
    LogFull,
    NoMemory,
}

/// What there is of a block.
#[derive(Default)]
struct Block {
    /// As much of it as is in memory, with its head.
    record: Vec<u8>,
    /// Where its head is in the log, if it is there.
    logged: Option<u64>,
    /// Whether it was seen to be whole in the log.
    checked: bool,
    /// Whether it came once more because it was damaged.
    again: bool,
}

/// The blocks of a file.
pub struct Blocks<F> {
    len: u64,
    blocks: Vec<Block>,
    /// Bytes that are here.
    have: u64,
    log: Option<F>,
    log_end: u64,
    /// What is waited for, in order.
    wanted: Vec<u32>,
    /// What was asked for: from the first block to before the second.
    asked: Vec<(u32, u32)>,
    /// What was read last: from the first block to before the second.
    last: (u32, u32),
    /// Blocks that were missing when they were to be read.
    missed: Vec<u32>,
}

const fn start(block: u32) -> u64 {
    block as u64 * BLOCK as u64
}

fn block_of(at: u64) -> u32 {
    u32::try_from(at / BLOCK as u64).unwrap_or(u32::MAX)
}

/// The head of a block.
fn head(block: u32, content: &[u8]) -> [u8; HEAD] {
    let sum = checksum(content) ^ block.wrapping_mul(0x9e37_79b1);
    (u64::from(sum) << 32 | u64::from(block)).to_le_bytes()
}

impl<F: Source + Sink> Blocks<F> {
    /// The blocks of a file of that length, none of which are here.
    pub fn new(len: u64) -> Self {
        let count = block_of(len + BLOCK as u64 - 1);
        let mut blocks = Vec::new();
        blocks.resize_with(count as usize, Block::default);
        Self {
            len,
            blocks,
            have: 0,
            log: None,
            log_end: 0,
            wanted: Vec::new(),
            asked: Vec::new(),
            last: (0, 0),
            missed: Vec::new(),
        }
    }

    /// Keeps what comes in a log from now on, which has so many bytes.
    pub fn attach(&mut self, mut log: F, log_len: u64) {
        let count = self.blocks.len();
        let mut bytes = [0; HEAD];
        while self.log_end + (HEAD as u64) < log_len && log.read(self.log_end, &mut bytes).is_ok() {
            let block = (u64::from_le_bytes(bytes) & 0xffff_ffff) as u32;
            let end = self.log_end + (HEAD + self.length(block)) as u64;
            if block as usize >= count || end > log_len {
                break;
            }
            // One that comes again was damaged before.
            self.drop(block);
            self.keep(block, Some(self.log_end));
            self.wanted.retain(|wanted| *wanted != block);
            self.log_end = end;
        }
        self.log = Some(log);
    }

    /// Where the log ends. What comes after that is of no use.
    pub const fn log_end(&self) -> u64 {
        self.log_end
    }

    /// Bytes that are here.
    pub const fn have(&self) -> u64 {
        self.have
    }

    #[inline(never)]
    fn length(&self, block: u32) -> usize {
        let rest = self.len.saturating_sub(start(block));
        usize::try_from(rest).map_or(BLOCK, |rest| rest.min(BLOCK))
    }

    /// Whether a block is here.
    #[inline(never)]
    pub fn has(&self, block: u32) -> bool {
        self.blocks.get(block as usize).is_some_and(|here| {
            here.logged.is_some() || here.record.len() == HEAD + self.length(block)
        })
    }

    /// A block is here from now on: in memory, or in the log.
    fn keep(&mut self, block: u32, logged: Option<u64>) {
        self.have += self.length(block) as u64;
        if let Some(here) = self.blocks.get_mut(block as usize) {
            here.logged = logged;
        }
    }

    /// A block is not here any more.
    fn drop(&mut self, block: u32) {
        if self.has(block) {
            self.have -= self.length(block) as u64;
        }
        if let Some(here) = self.blocks.get_mut(block as usize) {
            here.record = Vec::new();
            here.logged = None;
            here.checked = false;
        }
    }

    #[inline(never)]
    fn want(&mut self, block: u32) {
        if !self.wanted.contains(&block) {
            self.wanted.push(block);
        }
    }

    /// Has bytes come that are not here.
    pub fn want_bytes(&mut self, at: u64, len: u64) {
        for block in block_of(at)..=block_of((at + len).saturating_sub(1)) {
            if !self.has(block) {
                self.want(block);
            }
        }
    }

    /// Nothing is waited for any more.
    pub fn forget(&mut self) {
        self.wanted.clear();
    }

    /// The blocks that were missing in reads since the last call.
    pub fn take_missed(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.missed)
    }

    /// What was read last is damaged. Has it come once more, unless it already
    /// has. Whether it will.
    pub fn mend(&mut self) -> bool {
        let (first, end) = std::mem::take(&mut self.last);
        for block in first..end {
            match self.blocks.get_mut(block as usize) {
                Some(here) if !here.again => here.again = true,
                _ => return false,
            }
        }
        for block in first..end {
            self.drop(block);
            self.want(block);
        }
        first < end
    }

    /// Lets go of the log. What is in it is not here any more.
    pub fn leave_log(&mut self) {
        self.log = None;
        for block in 0..block_of(self.len) + 1 {
            let here = self.blocks.get(block as usize);
            if here.is_some_and(|here| here.logged.is_some()) {
                self.drop(block);
            }
        }
    }

    /// Whether a block is to be asked for, be it waited for or not.
    #[inline(never)]
    fn is_due(&self, block: u32, is_urgent: bool) -> bool {
        (block as usize) < self.blocks.len()
            && !self.has(block)
            && (!is_urgent || self.wanted.contains(&block))
            && !self
                .asked
                .iter()
                .any(|asked| (asked.0..asked.1).contains(&block))
    }

    /// What to ask for, and what to give up: for each request the block that it
    /// begins with, and the offsets of its first byte and of the one after its
    /// last, which are equal when it is to be given up. What is waited for comes
    /// first, and what else is missing only with so many requests at once.
    pub fn plan(&mut self, requests: usize) -> Vec<f64> {
        let mut plan = Vec::new();
        let is_urgent = !self.wanted.is_empty();
        let mut at = 0;
        while let Some(&(first, end)) = self.asked.get(at) {
            // What nobody waits for makes room. What has come of it is kept.
            let is_wanted = (first..end).any(|block| self.wanted.contains(&block));
            if is_wanted || !is_urgent && requests > 0 {
                at += 1;
            } else {
                self.asked.remove(at);
                plan.extend([f64::from(first), 0.0, 0.0]);
            }
        }
        let most = if is_urgent { MOST_REQUESTS } else { requests };
        while self.asked.len() < most
            && let Some((first, end)) = self.next(is_urgent)
        {
            self.asked.push((first, end));
            let from = start(first) + self.part(first) as u64;
            let until = start(end).min(self.len);
            plan.extend([f64::from(first), offset(from), offset(until)]);
        }
        plan
    }

    /// The blocks to ask for next.
    fn next(&self, is_urgent: bool) -> Option<(u32, u32)> {
        let mut first = 0;
        if is_urgent {
            let mut due = self.wanted.iter();
            first = *due.find(|block| self.is_due(**block, true))?;
        } else {
            // The last ones: the texts of all rows are at the end.
            for block in 0..=block_of(self.len) {
                if self.is_due(block, false) {
                    first = block;
                }
            }
        }
        let mut end = first;
        while end - first < MOST_BLOCKS && self.is_due(end, is_urgent) {
            end += 1;
        }
        while end - first < MOST_BLOCKS && first > 0 && self.is_due(first - 1, is_urgent) {
            first -= 1;
        }
        (first < end).then_some((first, end))
    }

    /// A request has ended, whatever came of it.
    pub fn ended(&mut self, block: u32) {
        self.asked.retain(|asked| asked.0 != block);
    }

    /// How much of a block is here that is not here as a whole.
    fn part(&self, block: u32) -> usize {
        let here = self.blocks.get(block as usize);
        here.map_or(0, |here| here.record.len().saturating_sub(HEAD))
    }

    /// Takes in bytes of the file, from an offset on.
    pub fn take(&mut self, at: u64, bytes: &[u8]) -> Taken {
        let mut taken = Taken::Kept;
        let mut at = at;
        let mut rest = bytes;
        loop {
            let block = block_of(at);
            let within = usize::try_from(at - start(block)).unwrap_or(BLOCK);
            let len = self.length(block).saturating_sub(within).min(rest.len());
            if len == 0 {
                return taken;
            }
            let (piece, later) = rest.split_at(len);
            let of_piece = self.take_piece(block, within, piece);
            if of_piece != Taken::Kept {
                taken = of_piece;
            }
            at += len as u64;
            rest = later;
        }
    }

    fn take_piece(&mut self, block: u32, within: usize, piece: &[u8]) -> Taken {
        // What does not go on where the beginning ends comes again.
        if self.has(block) || within > self.part(block) {
            return Taken::Kept;
        }
        let whole = HEAD + self.length(block);
        let Some(here) = self.blocks.get_mut(block as usize) else {
            return Taken::Kept;
        };
        let record = &mut here.record;
        if record.try_reserve_exact(whole - record.len()).is_err() {
            return Taken::NoMemory;
        }
        record.resize(HEAD + within, 0);
        record.extend_from_slice(piece);
        if record.len() < whole {
            return Taken::Kept;
        }
        let (into, content) = record.split_at_mut(HEAD);
        into.copy_from_slice(&head(block, content));
        self.wanted.retain(|wanted| *wanted != block);
        let Some(log) = &mut self.log else {
            self.keep(block, None);
            return Taken::Kept;
        };
        if log.append(record).is_err() {
            self.leave_log();
            self.keep(block, None);
            return Taken::LogFull;
        }
        here.record = Vec::new();
        here.checked = true;
        self.keep(block, Some(self.log_end));
        self.log_end += whole as u64;
        Taken::Kept
    }

    /// Reads a block in the log for the first time, to see whether it is whole.
    fn check(&mut self, block: u32) {
        let mut record = vec![0; HEAD + self.length(block)];
        if let Some(here) = self.blocks.get_mut(block as usize)
            && let (Some(at), Some(log)) = (here.logged, &mut self.log)
            && !here.checked
        {
            here.checked = log.read(at, &mut record).is_ok();
            let (read, content) = record.split_at(HEAD);
            if !here.checked || read != head(block, content) {
                self.drop(block);
            }
        }
    }
}

impl<F: Source + Sink> Source for Blocks<F> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let end = at + into.len() as u64;
        if into.is_empty() || end > self.len {
            return Err(SourceError::Failed);
        }
        self.last = (block_of(at), block_of(end - 1) + 1);
        let mut is_missing = false;
        for block in self.last.0..self.last.1 {
            self.check(block);
            if !self.has(block) {
                is_missing = true;
                self.missed.push(block);
                self.want(block);
            }
        }
        if is_missing {
            return Err(SourceError::Missing);
        }
        let mut rest = into;
        let mut at = at;
        while !rest.is_empty() {
            let block = block_of(at);
            let within = usize::try_from(at - start(block)).unwrap_or(BLOCK);
            let len = rest.len().min(self.length(block).saturating_sub(within));
            let (part, later) = rest.split_at_mut(len);
            let here = self.blocks.get(block as usize);
            let from = HEAD + within;
            match (here.and_then(|here| here.logged), &mut self.log) {
                (Some(logged), Some(log)) => log.read(logged + from as u64, part)?,
                _ => part.copy_from_slice(
                    here.and_then(|here| here.record.get(from..from + len))
                        .ok_or(SourceError::Failed)?,
                ),
            }
            at += len as u64;
            rest = later;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use reticula_table::source::SinkFailed;

    use super::*;

    /// A request to make, or one to give up, which is for no bytes.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Request {
        block: u32,
        from: u64,
        until: u64,
    }

    /// 160 blocks, and a part of another.
    const LEN: u64 = 160 * BLOCK as u64 + 1_000;

    /// A file with room for so many bytes.
    #[derive(Clone, Default)]
    struct Disk(Rc<RefCell<Vec<u8>>>, usize);

    impl Disk {
        fn new() -> Self {
            Self(Rc::default(), usize::MAX)
        }

        fn len(&self) -> u64 {
            self.0.borrow().len() as u64
        }

        fn blocks(&self) -> Blocks<Self> {
            let mut blocks = Blocks::new(LEN);
            blocks.attach(self.clone(), self.len());
            let end = usize::try_from(blocks.log_end()).unwrap();
            self.0.borrow_mut().truncate(end);
            blocks
        }
    }

    impl Source for Disk {
        fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
            self.0.borrow().as_slice().read(at, into)
        }
    }

    impl Sink for Disk {
        fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed> {
            let mut held = self.0.borrow_mut();
            if held.len() + bytes.len() > self.1 {
                return Err(SinkFailed);
            }
            held.extend_from_slice(bytes);
            Ok(())
        }
    }

    impl Blocks<Disk> {
        fn requests(&mut self, requests: usize) -> Vec<Request> {
            let plan = self.plan(requests);
            assert_eq!(plan.len() % 3, 0);
            plan.as_chunks::<3>()
                .0
                .iter()
                .map(|[block, from, until]| Request {
                    block: *block as u32,
                    from: *from as u64,
                    until: *until as u64,
                })
                .collect()
        }
    }

    fn memory() -> Blocks<Disk> {
        Blocks::new(LEN)
    }

    fn content(from: u64, until: u64) -> Vec<u8> {
        (from..until)
            .map(|at| (at.wrapping_mul(7) ^ (at >> 18)).to_le_bytes()[0])
            .collect()
    }

    /// Has some of a request arrive.
    fn arrive(blocks: &mut Blocks<Disk>, from: u64, until: u64) -> Taken {
        blocks.take(from, &content(from, until))
    }

    fn arrive_all(blocks: &mut Blocks<Disk>, request: Request) {
        assert_eq!(arrive(blocks, request.from, request.until), Taken::Kept);
        blocks.ended(request.block);
    }

    fn arrive_block(blocks: &mut Blocks<Disk>, block: u32) -> Taken {
        arrive(blocks, start(block), start(block + 1).min(LEN))
    }

    fn reads(blocks: &mut Blocks<Disk>, at: u64, len: usize) -> bool {
        let mut into = vec![0; len];
        match blocks.read(at, &mut into) {
            Ok(()) => {
                assert_eq!(into, content(at, at + len as u64));
                true
            }
            Err(error) => {
                assert_eq!(error, SourceError::Missing);
                false
            }
        }
    }

    /// The blocks of the requests of a plan.
    fn asked(plan: &[Request]) -> Vec<(u32, u32)> {
        plan.iter()
            .filter(|request| request.until > 0)
            .map(|request| (block_of(request.from), block_of(request.until - 1) + 1))
            .collect()
    }

    fn given_up(plan: &[Request]) -> Vec<u32> {
        plan.iter()
            .filter(|request| request.until == 0)
            .map(|request| request.block)
            .collect()
    }

    #[test]
    fn what_is_read_comes_in_whole_blocks() {
        let mut blocks = memory();
        assert!(!reads(&mut blocks, 1_000, 4));
        assert_eq!(blocks.take_missed(), [0]);
        let plan = blocks.requests(0);
        let expected = Request {
            block: 0,
            from: 0,
            until: BLOCK as u64,
        };
        assert_eq!(plan, [expected]);
        // Not twice.
        assert_eq!(blocks.requests(0), []);
        arrive_all(&mut blocks, plan[0]);
        assert!(reads(&mut blocks, 1_000, 4));
        assert_eq!(blocks.have(), BLOCK as u64);
        assert_eq!(blocks.requests(0), []);
    }

    #[test]
    fn the_last_block_ends_with_the_file() {
        let mut blocks = memory();
        assert!(!reads(&mut blocks, LEN - 64, 64));
        let plan = blocks.requests(0);
        assert_eq!((plan[0].from, plan[0].until), (start(160), LEN));
        arrive_all(&mut blocks, plan[0]);
        assert!(reads(&mut blocks, LEN - 64, 64));
        assert_eq!(blocks.have(), 1_000);
        assert_eq!(
            blocks.read(LEN - 64, &mut [0; 65]),
            Err(SourceError::Failed)
        );
        assert_eq!(blocks.read(0, &mut []), Err(SourceError::Failed));
    }

    #[test]
    fn neighbours_come_together_and_four_requests_at_once() {
        let mut blocks = memory();
        for block in [3, 4, 5, 10, 20, 30, 32, 12] {
            assert!(!reads(&mut blocks, start(block) + 5, 10));
        }
        let plan = blocks.requests(0);
        assert_eq!(asked(&plan), [(3, 6), (10, 11), (20, 21), (30, 31)]);
        arrive_all(&mut blocks, plan[1]);
        assert_eq!(asked(&blocks.requests(0)), [(32, 33)]);
    }

    #[test]
    fn a_request_is_not_for_too_much() {
        let mut blocks = memory();
        for block in 0..160 {
            assert!(!reads(&mut blocks, start(block), 1));
        }
        assert_eq!(asked(&blocks.requests(0)), [(0, 64), (64, 128), (128, 160)]);
    }

    #[test]
    fn a_read_across_blocks_needs_all_of_them() {
        let mut blocks = memory();
        let at = start(2) - 2;
        assert!(!reads(&mut blocks, at, 4));
        assert_eq!(blocks.take_missed(), [1, 2]);
        assert_eq!(asked(&blocks.requests(0)), [(1, 3)]);
        assert_eq!(arrive_block(&mut blocks, 1), Taken::Kept);
        assert!(!reads(&mut blocks, at, 4));
        assert_eq!(arrive_block(&mut blocks, 2), Taken::Kept);
        assert!(reads(&mut blocks, at, 4));
    }

    #[test]
    fn what_is_not_wanted_any_more_is_given_up_and_goes_on_later() {
        let mut blocks = memory();
        assert!(!reads(&mut blocks, 0, 3 * BLOCK));
        let first = blocks.requests(0)[0];
        arrive(&mut blocks, 0, start(1) + 1_000);
        assert_eq!(blocks.have(), BLOCK as u64);
        blocks.forget();
        assert!(!reads(&mut blocks, start(20), 1));
        let plan = blocks.requests(0);
        assert_eq!(
            (given_up(&plan), asked(&plan)),
            (vec![first.block], vec![(20, 21)])
        );
        arrive_all(&mut blocks, plan[1]);
        assert!(!reads(&mut blocks, start(1), 1));
        let again = blocks.requests(0)[0];
        assert_eq!((again.from, again.until), (start(1) + 1_000, start(2)));
        arrive_all(&mut blocks, again);
        assert!(reads(&mut blocks, start(1), 2_000));
    }

    #[test]
    fn what_is_still_wanted_goes_on() {
        let mut blocks = memory();
        assert!(!reads(&mut blocks, 0, 1));
        blocks.requests(0);
        blocks.forget();
        assert!(!reads(&mut blocks, 0, 1));
        assert!(!reads(&mut blocks, start(9), 1));
        let plan = blocks.requests(0);
        assert_eq!((given_up(&plan), asked(&plan)), (vec![], vec![(9, 10)]));
    }

    #[test]
    fn beginnings_are_kept() {
        let mut blocks = memory();
        for block in 0..12 {
            arrive(&mut blocks, start(block), start(block) + 100);
        }
        assert_eq!(blocks.have(), 0);
        assert!(!reads(&mut blocks, start(3), 1) && !reads(&mut blocks, start(11), 1));
        let from: Vec<u64> = blocks
            .requests(0)
            .iter()
            .map(|request| request.from)
            .collect();
        assert_eq!(from, [start(3) + 100, start(11) + 100]);
    }

    #[test]
    fn bytes_that_do_not_go_on_where_others_end_are_not_kept() {
        let mut blocks = memory();
        arrive(&mut blocks, 100, start(1));
        assert!(!reads(&mut blocks, 0, 1));
        arrive(&mut blocks, 0, 200);
        arrive(&mut blocks, 300, start(1));
        assert_eq!(blocks.requests(0)[0].from, 200);
        // These overlap with what is here.
        arrive(&mut blocks, 150, start(1));
        assert!(reads(&mut blocks, 0, BLOCK));
        // Once more, and beyond the end.
        arrive(&mut blocks, 0, start(1));
        assert_eq!(blocks.take(LEN, &[1]), Taken::Kept);
        assert_eq!(blocks.have(), BLOCK as u64);
    }

    #[test]
    fn more_may_come_than_was_asked_for() {
        let mut blocks = memory();
        assert!(!reads(&mut blocks, start(5), 1));
        let request = blocks.requests(0)[0];
        assert_eq!(arrive(&mut blocks, 0, request.until), Taken::Kept);
        blocks.ended(request.block);
        assert_eq!(blocks.have(), 6 * BLOCK as u64);
        assert!(reads(&mut blocks, 0, 6 * BLOCK));
    }

    #[test]
    fn what_nobody_waits_for_comes_from_the_end() {
        let mut blocks = memory();
        let plan = blocks.requests(1);
        assert_eq!(asked(&plan), [(97, 161)]);
        arrive_all(&mut blocks, plan[0]);
        let plan = blocks.requests(1);
        assert_eq!(asked(&plan), [(33, 97)]);
        // What is waited for goes before.
        assert!(!reads(&mut blocks, 0, 1));
        let wanted = blocks.requests(1);
        assert_eq!(
            (given_up(&wanted), asked(&wanted)),
            (vec![33], vec![(0, 1)])
        );
        arrive_all(&mut blocks, wanted[1]);
        for rest in [(33, 97), (1, 33)] {
            let plan = blocks.requests(1);
            assert_eq!(asked(&plan), [rest]);
            arrive_all(&mut blocks, plan[0]);
        }
        assert_eq!(blocks.have(), LEN);
        // With more at once, and no more when it is not to come.
        let mut blocks = memory();
        assert_eq!(asked(&blocks.requests(4)), [(97, 161), (33, 97), (0, 33)]);
        assert_eq!(given_up(&blocks.requests(0)), [97, 33, 0]);
    }

    #[test]
    fn what_is_damaged_comes_once_more_and_no_more_than_once() {
        let mut blocks = memory();
        assert!(!blocks.mend());
        arrive_block(&mut blocks, 0);
        assert!(reads(&mut blocks, 100, 4));
        assert!(blocks.mend());
        assert!(!reads(&mut blocks, 100, 4));
        let plan = blocks.requests(0);
        assert_eq!(asked(&plan), [(0, 1)]);
        arrive_all(&mut blocks, plan[0]);
        assert!(reads(&mut blocks, 100, 4));
        assert!(!blocks.mend());
        assert!(reads(&mut blocks, 100, 4));
    }

    #[test]
    fn a_log_has_what_came_in_any_order_also_when_it_is_opened_again() {
        let disk = Disk::new();
        let mut first = disk.blocks();
        for block in [160, 0] {
            assert_eq!(arrive_block(&mut first, block), Taken::Kept);
        }
        assert!(reads(&mut first, LEN - 10, 10));
        let mut second = disk.blocks();
        assert!(second.has(0) && second.has(160) && !second.has(1));
        assert!(reads(&mut second, 12_345, 100) && reads(&mut second, LEN - 10, 10));
        assert_eq!(second.have(), BLOCK as u64 + 1_000);
        assert!(!reads(&mut second, start(1), 1));
    }

    #[test]
    fn a_log_goes_on_before_a_record_that_is_torn() {
        for torn in [1, HEAD - 1, HEAD, HEAD + 1, BLOCK] {
            let disk = Disk::new();
            let mut first = disk.blocks();
            arrive_block(&mut first, 160);
            let whole = disk.len();
            arrive_block(&mut first, 1);
            disk.0
                .borrow_mut()
                .truncate(usize::try_from(whole).unwrap() + torn);
            let mut second = disk.blocks();
            assert_eq!(disk.len(), whole, "{torn}");
            assert!(!second.has(1) && second.has(160));
            arrive_block(&mut second, 0);
            let mut third = disk.blocks();
            assert!(reads(&mut third, 7, 7) && !third.has(1) && third.has(160));
        }
    }

    #[test]
    fn a_log_ends_before_what_is_no_record() {
        let disk = Disk::new();
        let mut first = disk.blocks();
        arrive_block(&mut first, 0);
        arrive_block(&mut first, 1);
        disk.0.borrow_mut()[HEAD + BLOCK] = 0xff;
        let second = disk.blocks();
        assert!(second.has(0) && !second.has(1));
    }

    #[test]
    fn a_block_that_is_damaged_in_the_log_comes_again() {
        for at in [HEAD + 100, 5, 0] {
            let disk = Disk::new();
            let mut first = disk.blocks();
            arrive_block(&mut first, 2);
            disk.0.borrow_mut()[at] ^= 1;
            let mut second = disk.blocks();
            assert!(!reads(&mut second, start(2), 1) && !reads(&mut second, start(3), 1));
            arrive_block(&mut second, 2);
            assert!(reads(&mut second, start(2), 9));
            let mut third = disk.blocks();
            assert!(reads(&mut third, start(2), 9));
        }
    }

    #[test]
    fn a_log_without_room_is_left() {
        let disk = Disk(Rc::default(), 2 * (HEAD + BLOCK) + 10);
        let mut blocks = disk.blocks();
        assert_eq!(arrive_block(&mut blocks, 0), Taken::Kept);
        assert_eq!(arrive_block(&mut blocks, 1), Taken::Kept);
        assert_eq!(arrive_block(&mut blocks, 2), Taken::LogFull);
        // What came last is in memory.
        assert!(!blocks.has(0) && !blocks.has(1));
        assert!(reads(&mut blocks, start(2), 100));
        assert_eq!(arrive_block(&mut blocks, 0), Taken::Kept);
        assert!(reads(&mut blocks, 0, 100));
        assert_eq!(blocks.have(), 2 * BLOCK as u64);
    }
}
