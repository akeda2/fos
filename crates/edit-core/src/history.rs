//! The undo log: one slice of records, `[kind, joined, pos u32, len u32, bytes..., size u32]`.
//! The trailing size lets undo walk backwards. Records before `at` can be undone, records from
//! `at` to `end` can be redone, and a new edit drops the redo part. A `joined` record is undone
//! and redone together with the record before it (a replacement, a run of typing).

pub const INSERT: u8 = 0;
pub const DELETE: u8 = 1;
const HEADER: usize = 10;
const TRAILER: usize = 4;

pub struct History<'a> {
    log: &'a mut [u8],
    at: usize,
    end: usize,
}

#[derive(Clone, Copy)]
pub struct Record {
    pub kind: u8,
    pub joined: bool,
    pub pos: usize,
    pub len: usize,
    /// Where the record starts in the log and how big it is.
    pub offset: usize,
    pub size: usize,
}

impl<'a> History<'a> {
    pub fn new(log: &'a mut [u8]) -> Self {
        Self { log, at: 0, end: 0 }
    }

    pub fn clear(&mut self) {
        self.at = 0;
        self.end = 0;
    }

    pub fn can_undo(&self) -> bool {
        self.at > 0
    }

    pub fn can_redo(&self) -> bool {
        self.at < self.end
    }

    /// Room for a record carrying `len` bytes; the data slice to fill. History that is full is
    /// forgotten (the edit still happens), so an editor never refuses to type.
    pub fn reserve(&mut self, kind: u8, joined: bool, pos: usize, len: usize) -> Option<&mut [u8]> {
        let size = HEADER + len + TRAILER;
        self.end = self.at;
        if self.at + size > self.log.len() {
            self.clear();
            if size > self.log.len() {
                return None;
            }
        }
        let o = self.at;
        self.log[o] = kind;
        self.log[o + 1] = joined as u8;
        self.log[o + 2..o + 6].copy_from_slice(&(pos as u32).to_le_bytes());
        self.log[o + 6..o + 10].copy_from_slice(&(len as u32).to_le_bytes());
        self.log[o + HEADER + len..o + size].copy_from_slice(&(size as u32).to_le_bytes());
        self.at += size;
        self.end = self.at;
        Some(&mut self.log[o + HEADER..o + HEADER + len])
    }

    /// Marks the newest record as joined to the one before it.
    pub fn join_last(&mut self) {
        if let Some(rec) = self.last() {
            self.log[rec.offset + 1] = 1;
        }
    }

    fn read(&self, offset: usize) -> Record {
        let le = |a: usize| u32::from_le_bytes([self.log[a], self.log[a + 1], self.log[a + 2], self.log[a + 3]]) as usize;
        let len = le(offset + 6);
        Record {
            kind: self.log[offset],
            joined: self.log[offset + 1] != 0,
            pos: le(offset + 2),
            len,
            offset,
            size: HEADER + len + TRAILER,
        }
    }

    /// The newest undoable record.
    pub fn last(&self) -> Option<Record> {
        if self.at == 0 {
            return None;
        }
        let size = u32::from_le_bytes([
            self.log[self.at - 4],
            self.log[self.at - 3],
            self.log[self.at - 2],
            self.log[self.at - 1],
        ]) as usize;
        Some(self.read(self.at - size))
    }

    /// Steps back over the newest record and returns it.
    pub fn pop(&mut self) -> Option<Record> {
        let rec = self.last()?;
        self.at = rec.offset;
        Some(rec)
    }

    /// The next redoable record, stepping over it.
    pub fn advance(&mut self) -> Option<Record> {
        if self.at >= self.end {
            return None;
        }
        let rec = self.read(self.at);
        self.at += rec.size;
        Some(rec)
    }

    /// Whether the record after the redo point is joined to the one before it.
    pub fn next_joined(&self) -> bool {
        self.at < self.end && self.log[self.at + 1] != 0
    }

    pub fn data(&self, rec: &Record) -> &[u8] {
        &self.log[rec.offset + HEADER..rec.offset + HEADER + rec.len]
    }
}
