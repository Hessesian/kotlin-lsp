//! Async task orchestration utilities (SOLID refactoring)
//!
//! Single responsibility: execute async work items concurrently and collect results.
//! Separated from indexer logic for testability.

use std::sync::Arc;
use tokio::sync::Semaphore;

/// Execute work items concurrently with semaphore throttling.
/// 
/// This is a pure async orchestrator - it doesn't know about indexing,
/// just runs async functions and collects results.
/// 
/// # Arguments
/// * `items` - Work items to process
/// * `semaphore` - Controls max concurrency
/// * `worker` - Async function that processes one item
/// 
/// # Returns
/// Vector of results in same order as input items
pub async fn run_concurrent<T, R, F, Fut>(
    items: Vec<T>,
    semaphore: Arc<Semaphore>,
    worker: F,
) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T, Arc<Semaphore>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    let worker = Arc::new(worker);
    let mut handles = Vec::with_capacity(items.len());
    
    // Spawn all tasks immediately - semaphore is only for throttling spawn_blocking inside worker
    for item in items {
        let sem = Arc::clone(&semaphore);
        let worker = Arc::clone(&worker);
        
        handles.push(tokio::spawn(async move {
            worker(item, sem).await
        }));
    }
    
    // Collect results
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => panic!("Task panicked: {}", e),
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_concurrent_simple() {
        let items = vec![1, 2, 3, 4, 5];
        let sem = Arc::new(Semaphore::new(2)); // max 2 concurrent
        
        let results = run_concurrent(items, sem, |n, _sem| async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            n * 2
        }).await;
        
        assert_eq!(results, vec![2, 4, 6, 8, 10]);
    }
    
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_concurrent_respects_semaphore() {
        let items: Vec<usize> = (0..10).collect();
        let sem = Arc::new(Semaphore::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        
        let active_clone = Arc::clone(&active);
        let max_clone = Arc::clone(&max_concurrent);
        
        // Worker acquires semaphore permit to throttle concurrent work
        let results = run_concurrent(items, sem, move |n, sem| {
            let active = Arc::clone(&active_clone);
            let max_concurrent = Arc::clone(&max_clone);
            async move {
                let _permit = sem.acquire().await.unwrap();  // Acquire inside worker
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                n
            }
        }).await;
        
        assert_eq!(results.len(), 10);
        assert!(max_concurrent.load(Ordering::SeqCst) <= 2, 
            "Max concurrent was {}, expected <= 2", max_concurrent.load(Ordering::SeqCst));
    }
    
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_async_io_plus_spawn_blocking() {
        // Simulate real indexing pattern: async I/O (all parallel) + spawn_blocking (throttled)
        let items: Vec<usize> = (0..20).collect();
        let sem = Arc::new(Semaphore::new(4)); // Throttle spawn_blocking to 4
        let blocking_active = Arc::new(AtomicUsize::new(0));
        let max_blocking = Arc::new(AtomicUsize::new(0));
        
        let blocking_clone = Arc::clone(&blocking_active);
        let max_clone = Arc::clone(&max_blocking);
        
        let results = run_concurrent(items, sem, move |n, sem| {
            let blocking = Arc::clone(&blocking_clone);
            let max_concurrent = Arc::clone(&max_clone);
            async move {
                // Simulate async I/O (file read) - all 20 happen in parallel
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                
                // Acquire permit before spawn_blocking (like real parsing)
                let _permit = sem.acquire().await.unwrap();
                
                // Simulate CPU-bound work in spawn_blocking
                let result = tokio::task::spawn_blocking(move || {
                    let current = blocking.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    blocking.fetch_sub(1, Ordering::SeqCst);
                    n * 2
                }).await.unwrap();
                
                result
            }
        }).await;
        
        assert_eq!(results.len(), 20);
        // Verify spawn_blocking was throttled to max 4 concurrent
        let max = max_blocking.load(Ordering::SeqCst);
        assert!(max <= 4, "Max concurrent spawn_blocking was {}, expected <= 4", max);
        // Should have some parallelism (not serialized to 1)
        assert!(max >= 2, "Max concurrent spawn_blocking was {}, expected >= 2", max);
    }
}
