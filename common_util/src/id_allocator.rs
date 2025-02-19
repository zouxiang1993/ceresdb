// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

use std::future::Future;

use tokio::sync::RwLock;

use crate::error::GenericResult;

struct Inner {
    last_id: u64,
    max_id: u64,
    alloc_step: u64,
}

impl Inner {
    /// New a allocator.
    pub fn new(last_id: u64, max_id: u64, alloc_step: u64) -> Self {
        assert!(alloc_step > 0);
        Self {
            last_id,
            max_id,
            alloc_step,
        }
    }

    /// Alloc id.
    pub async fn alloc_id<F, T>(&mut self, persist_next_max_id: F) -> GenericResult<u64>
    where
        F: FnOnce(u64) -> T,
        T: Future<Output = GenericResult<()>>,
    {
        if self.last_id < self.max_id {
            self.last_id += 1;
            return Ok(self.last_id);
        }

        // Update new max id.
        let next_max_id = self.last_id + self.alloc_step;

        // persist new max id.
        persist_next_max_id(next_max_id).await?;

        // Update memory.
        self.max_id = next_max_id;

        self.last_id += 1;
        Ok(self.last_id)
    }
}

pub struct IdAllocator {
    inner: RwLock<Inner>,
}

impl IdAllocator {
    /// New a id allocator.
    pub fn new(last_id: u64, max_id: u64, alloc_step: u64) -> Self {
        Self {
            inner: RwLock::new(Inner::new(last_id, max_id, alloc_step)),
        }
    }

    /// Alloc id.
    pub async fn alloc_id<F, T>(&self, persist_next_max_id: F) -> GenericResult<u64>
    where
        F: FnOnce(u64) -> T,
        T: Future<Output = GenericResult<()>>,
    {
        self.inner.write().await.alloc_id(persist_next_max_id).await
    }
}

#[cfg(test)]

mod test {
    use tokio::runtime::Runtime;

    use super::IdAllocator;

    #[test]
    fn test_alloc_id() {
        let rt = Runtime::new().unwrap();
        let allocator = IdAllocator::new(0, 0, 100);

        rt.block_on(async move {
            let persist_max_file_id = move |next_max_file_id| async move {
                assert_eq!(next_max_file_id, 100);
                Ok(())
            };

            for i in 1..=100 {
                let res = allocator.alloc_id(persist_max_file_id).await.unwrap();
                assert_eq!(res, i);
            }

            let persist_max_file_id = move |next_max_file_id| async move {
                assert_eq!(next_max_file_id, 200);
                Ok(())
            };

            for i in 101..=200 {
                let res = allocator.alloc_id(persist_max_file_id).await.unwrap();
                assert_eq!(res, i);
            }
        });
    }
}
