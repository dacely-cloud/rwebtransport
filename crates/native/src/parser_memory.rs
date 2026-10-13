// SPDX-License-Identifier: Apache-2.0
//! Allocation accounting shared by every receive parser on a server.

use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub(crate) struct ParserMemory {
    limit: usize,
    used: AtomicUsize,
    parent: Option<Arc<ParserMemory>>,
}

impl ParserMemory {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            parent: None,
        })
    }

    pub(crate) fn child(parent: Arc<Self>, limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            parent: Some(parent),
        })
    }

    fn reserve(&self, bytes: usize) -> bool {
        let reserved = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&next| next <= self.limit)
            })
            .is_ok();
        if !reserved {
            return false;
        }
        if let Some(parent) = &self.parent {
            if !parent.reserve(bytes) {
                self.used.fetch_sub(bytes, Ordering::Relaxed);
                return false;
            }
        }
        true
    }

    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
        if let Some(parent) = &self.parent {
            parent.release(bytes);
        }
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
}

pub(crate) struct ParserBuffer {
    data: Vec<u8>,
    memory: Arc<ParserMemory>,
}

impl ParserBuffer {
    pub(crate) fn new(memory: Arc<ParserMemory>) -> Self {
        Self {
            data: Vec::new(),
            memory,
        }
    }

    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let needed = self.data.len().checked_add(bytes.len()).ok_or(())?;
        if needed > self.data.capacity() {
            let capacity = needed.checked_next_power_of_two().ok_or(())?;
            // Reserve the whole replacement before allocation: the old and new
            // buffers coexist during growth, and both must fit the budget.
            if !self.memory.reserve(capacity) {
                return Err(());
            }
            let mut replacement = Vec::new();
            if replacement.try_reserve_exact(capacity).is_err() {
                self.memory.release(capacity);
                return Err(());
            }
            // Account for any excess capacity supplied by the allocator too.
            let extra = replacement.capacity() - capacity;
            if extra != 0 && !self.memory.reserve(extra) {
                drop(replacement);
                self.memory.release(capacity);
                return Err(());
            }
            replacement.extend_from_slice(&self.data);
            let old = std::mem::replace(&mut self.data, replacement);
            let old_capacity = old.capacity();
            drop(old);
            self.memory.release(old_capacity);
        }
        self.data.extend_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn consume(&mut self, bytes: usize) {
        if bytes == self.data.len() {
            self.clear();
        } else {
            self.data.drain(..bytes);
        }
    }

    pub(crate) fn copy_range(&self, range: std::ops::Range<usize>) -> Result<Self, ()> {
        let mut copy = Self::new(self.memory.clone());
        copy.extend_from_slice(&self.data[range])?;
        Ok(copy)
    }

    pub(crate) fn clear(&mut self) {
        let old = std::mem::take(&mut self.data);
        let capacity = old.capacity();
        drop(old);
        self.memory.release(capacity);
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.data.capacity()
    }
}

impl Deref for ParserBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for ParserBuffer {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_enforce_local_and_shared_limits_and_roll_back_refusals() {
        let shared = ParserMemory::new(128);
        let local = ParserMemory::child(shared.clone(), 64);
        let other = ParserMemory::child(shared.clone(), 128);
        let mut a = ParserBuffer::new(local.clone());
        let mut b = ParserBuffer::new(other.clone());
        a.extend_from_slice(&[0; 64]).unwrap();
        assert!(a.extend_from_slice(&[0; 1]).is_err());
        b.extend_from_slice(&[0; 64]).unwrap();
        assert!(b.extend_from_slice(&[0; 1]).is_err());
        assert_eq!(shared.used(), 128);
        assert_eq!(local.used(), 64);
        assert_eq!(other.used(), 64);
        drop(a);
        drop(b);
        assert_eq!(shared.used(), 0);
        assert_eq!(local.used(), 0);
        assert_eq!(other.used(), 0);
    }

    #[test]
    fn charges_capacity_and_releases_on_consumption_and_drop() {
        let memory = ParserMemory::new(128);
        let mut a = ParserBuffer::new(memory.clone());
        a.extend_from_slice(&[1; 33]).unwrap();
        assert_eq!(memory.used(), a.capacity());
        a.consume(32);
        assert_eq!(memory.used(), a.capacity()); // remaining one byte retains capacity
        let mut b = ParserBuffer::new(memory.clone());
        b.extend_from_slice(&[2; 64]).unwrap();
        assert!(a.extend_from_slice(&[3; 64]).is_err());
        assert_eq!(a.len(), 1); // refusal does not mutate the existing buffer
        a.consume(1);
        assert_eq!(memory.used(), b.capacity());
        drop(b);
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn growth_reserves_old_and_new_allocations_before_allocating() {
        let memory = ParserMemory::new(100);
        let mut buf = ParserBuffer::new(memory.clone());
        buf.extend_from_slice(&[0; 32]).unwrap();
        buf.extend_from_slice(&[0; 1]).unwrap();
        assert_eq!(memory.used(), 64);
        assert!(buf.extend_from_slice(&[0; 32]).is_err());
        assert_eq!(memory.used(), 64);
        buf.clear();
        assert_eq!(memory.used(), 0);
    }
}
