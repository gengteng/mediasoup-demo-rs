use mediasoup::prelude::*;
// use mediasoup::worker::WorkerId;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct WorkerPool {
    workers: Vec<Worker>,
    manager: WorkerManager,
    index: Arc<AtomicUsize>,
}

impl WorkerPool {
    pub async fn new(size: usize, settings: WorkerSettings) -> anyhow::Result<Self> {
        if size == 0 {
            anyhow::bail!("Worker pool size must not be 0.");
        }

        let manager = WorkerManager::default();
        let index = Arc::new(AtomicUsize::default());
        let mut workers = Vec::with_capacity(size);

        for _ in 0..size {
            workers.push(manager.create_worker(settings.clone()).await?);
        }

        Ok(Self {
            workers,
            manager,
            index,
        })
    }

    pub fn next(&self) -> Worker {
        let index = self.index.fetch_add(1, Ordering::Relaxed);
        let len = self.workers.len();
        self.workers[index % len].clone()
    }

    // pub fn get(&self, worker_id: WorkerId) -> Option<Worker> {
    //     self.workers.iter().find(|w| w.id() == worker_id).cloned()
    // }
}
