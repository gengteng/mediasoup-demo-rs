use mediasoup::worker::{Worker, WorkerSettings};
use mediasoup::worker_manager::WorkerManager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _workers = init_workers().await?;

    Ok(())
}

async fn init_workers() -> anyhow::Result<Vec<Worker>> {
    let cpus = num_cpus::get();
    let wm = WorkerManager::new();

    let mut vec = Vec::with_capacity(cpus);
    for _ in 0..cpus {
        vec.push(wm.create_worker(WorkerSettings::default()).await?);
    }

    Ok(vec)
}
