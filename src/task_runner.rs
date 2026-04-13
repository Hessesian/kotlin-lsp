/// Async task orchestration utilities (SOLID refactoring)
/// 
/// Single responsibility: execute async work items concurrently and collect results.
/// Separated from indexer logic for testability.

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
    
    // CRITICAL FIX: Acquire permit BEFORE spawning task to prevent spawn_blocking pool exhaustion.
    // Old behavior: spawn all tasks → each acquires permit inside → all call spawn_blocking → deadlock.
    // New behavior: acquire permit → spawn task → task calls spawn_blocking with guarantee of execution.
    for item in items {
        let sem = Arc::clone(&semaphore);
        let sem_for_worker = Arc::clone(&sem);
        let permit = sem.acquire_owned().await.unwrap();
        let worker = Arc::clone(&worker);
        
        handles.push(tokio::spawn(async move {
            let _permit = permit; // Hold permit for task lifetime
            worker(item, sem_for_worker).await
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
        
        let results = run_concurrent(items, sem, move |n, _sem| {
            let active = Arc::clone(&active_clone);
            let max_concurrent = Arc::clone(&max_clone);
            async move {
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
}
