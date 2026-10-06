//! A ring of the most recent audio, kept from the moment the tape starts so that a take can begin a few seconds
//! before the operator pressed record (what handheld recorders call pre-record or pre-roll).
//!
//! It works on whole frames over memory the caller provides (the tape allocates it), and never loses its place when
//! a push is not a whole number of frames. When the take starts, `slices` gives the stored audio oldest first in
//! two pieces (the ring wraps), to be written ahead of the live audio.

pub struct PreRoll<'a> {
    buffer: &'a mut [u8],
    frame: usize,
    /// Where the next byte goes, and how many bytes are valid (up to the capacity).
    head: usize,
    filled: usize,
}

impl<'a> PreRoll<'a> {
    /// A ring over `buffer` for frames of `frame` bytes; the usable size is the buffer's whole frames.
    pub fn new(buffer: &'a mut [u8], frame: usize) -> PreRoll<'a> {
        let usable = buffer.len() / frame.max(1) * frame.max(1);
        let (buffer, _) = buffer.split_at_mut(usable);
        PreRoll { buffer, frame: frame.max(1), head: 0, filled: 0 }
    }

    /// Bytes for `seconds` of audio at `rate` (to size the allocation).
    pub fn bytes_for(seconds: u32, rate: u32, frame: usize) -> usize {
        seconds as usize * rate as usize * frame
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Bytes held now.
    pub fn len(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.filled = 0;
    }

    /// Appends `data`, dropping the oldest audio when full. `data` should be whole frames (the device delivers them).
    pub fn push(&mut self, data: &[u8]) {
        let capacity = self.buffer.len();
        if capacity == 0 {
            return;
        }
        // Only the last `capacity` bytes can survive.
        let data = if data.len() > capacity { &data[data.len() - capacity..] } else { data };
        let first = (capacity - self.head).min(data.len());
        self.buffer[self.head..self.head + first].copy_from_slice(&data[..first]);
        self.buffer[..data.len() - first].copy_from_slice(&data[first..]);
        self.head = (self.head + data.len()) % capacity;
        self.filled = (self.filled + data.len()).min(capacity);
        // Stay on a frame boundary: the oldest byte must start a frame.
        let excess = self.filled % self.frame;
        self.filled -= excess;
    }

    /// The stored audio, oldest first, limited to the last `max_bytes` (rounded down to whole frames).
    pub fn slices(&self, max_bytes: usize) -> (&[u8], &[u8]) {
        let capacity = self.buffer.len();
        let wanted = self.filled.min(max_bytes) / self.frame * self.frame;
        if wanted == 0 {
            return (&[], &[]);
        }
        let start = (self.head + capacity - wanted) % capacity;
        if start + wanted <= capacity {
            (&self.buffer[start..start + wanted], &[])
        } else {
            (&self.buffer[start..], &self.buffer[..wanted - (capacity - start)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(ring: &PreRoll, max: usize) -> Vec<u8> {
        let (a, b) = ring.slices(max);
        [a, b].concat()
    }

    #[test]
    fn keeps_the_latest() {
        let mut storage = [0u8; 12];
        let mut ring = PreRoll::new(&mut storage, 2);
        ring.push(&[1, 2, 3, 4]);
        assert_eq!(collect(&ring, 100), [1, 2, 3, 4]);
        ring.push(&[5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(ring.len(), 12);
        assert_eq!(collect(&ring, 100), [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(collect(&ring, 4), [11, 12, 13, 14]);
    }

    #[test]
    fn larger_push_than_capacity() {
        let mut storage = [0u8; 8];
        let mut ring = PreRoll::new(&mut storage, 4);
        let data: Vec<u8> = (0..20).collect();
        ring.push(&data);
        assert_eq!(collect(&ring, 100), (12..20).collect::<Vec<u8>>());
    }

    #[test]
    fn wraps_in_two_slices() {
        let mut storage = [0u8; 8];
        let mut ring = PreRoll::new(&mut storage, 2);
        ring.push(&[1, 2, 3, 4, 5, 6]);
        ring.push(&[7, 8, 9, 10]);
        let (a, b) = ring.slices(100);
        assert!(!b.is_empty());
        assert_eq!([a, b].concat(), [3, 4, 5, 6, 7, 8, 9, 10]);
    }
}
