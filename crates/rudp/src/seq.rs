//! Wrapping 16-bit sequence numbers and a fixed-size ring buffer keyed by them.

/// `a` is newer than `b` under wraparound (RFC 1982 style, half-range window).
#[inline]
pub fn seq_greater(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

#[inline]
pub fn seq_less(a: u16, b: u16) -> bool {
    seq_greater(b, a)
}

/// Ring buffer indexed by sequence number; each slot remembers which sequence owns it,
/// so stale entries from a previous wrap are never mistaken for current ones.
pub struct SeqBuffer<T> {
    slots: Vec<Option<(u16, T)>>,
}

impl<T> SeqBuffer<T> {
    pub fn new(size: usize) -> Self {
        assert!(size > 0 && size <= 0x8000, "SeqBuffer size must be in 1..=32768");
        let mut slots = Vec::with_capacity(size);
        slots.resize_with(size, || None);
        Self { slots }
    }

    #[inline]
    fn index(&self, seq: u16) -> usize {
        seq as usize % self.slots.len()
    }

    pub fn insert(&mut self, seq: u16, value: T) {
        let i = self.index(seq);
        self.slots[i] = Some((seq, value));
    }

    pub fn get(&self, seq: u16) -> Option<&T> {
        match &self.slots[self.index(seq)] {
            Some((s, v)) if *s == seq => Some(v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, seq: u16) -> Option<&mut T> {
        let i = self.index(seq);
        match &mut self.slots[i] {
            Some((s, v)) if *s == seq => Some(v),
            _ => None,
        }
    }

    pub fn remove(&mut self, seq: u16) -> Option<T> {
        let i = self.index(seq);
        match &self.slots[i] {
            Some((s, _)) if *s == seq => self.slots[i].take().map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn contains(&self, seq: u16) -> bool {
        self.get(seq).is_some()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraparound_ordering() {
        assert!(seq_greater(1, 0));
        assert!(seq_greater(0, 65535));
        assert!(seq_less(65535, 0));
        assert!(!seq_greater(5, 5));
        assert!(seq_greater(0x7fff, 0));
        assert!(!seq_greater(0x8000, 0));
    }

    #[test]
    fn stale_slot_not_returned() {
        let mut b = SeqBuffer::new(4);
        b.insert(1, "a");
        assert_eq!(b.get(5), None, "slot 1 is owned by seq 1, not 5");
        b.insert(5, "b");
        assert_eq!(b.get(1), None);
        assert_eq!(b.get(5), Some(&"b"));
    }
}
