// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

mod client;
mod config;
pub mod deadlock;
mod metrics;
pub mod waiter_manager;

pub use self::config::Config;
pub use self::deadlock::Service as DeadlockService;

use self::deadlock::{Detector, Scheduler as DetectorScheduler};
use self::waiter_manager::{Scheduler as WaiterMgrScheduler, WaiterManager};

use crate::raftstore::coprocessor::CoprocessorHost;
use crate::server::resolve::StoreAddrResolver;
use crate::server::{Error, Result};
use crate::storage::{lock_manager::Lock, txn::ProcessResult, LockMgr, StorageCb};
use pd_client::RpcClient;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use tikv_util::security::SecurityManager;
use tikv_util::worker::FutureWorker;

/// `LockManager` has two components working in two threads:
///   * One is the `WaiterManager` which manages transactions waiting for locks.
///   * The other one is the `Detector` which detects deadlocks between transactions.
pub struct LockManager {
    waiter_mgr_worker: Option<FutureWorker<waiter_manager::Task>>,
    detector_worker: Option<FutureWorker<deadlock::Task>>,

    waiter_mgr_scheduler: WaiterMgrScheduler,
    detector_scheduler: DetectorScheduler,

    waiter_count: Arc<AtomicUsize>,
}

impl Clone for LockManager {
    fn clone(&self) -> Self {
        Self {
            waiter_mgr_worker: None,
            detector_worker: None,
            waiter_mgr_scheduler: self.waiter_mgr_scheduler.clone(),
            detector_scheduler: self.detector_scheduler.clone(),
            waiter_count: self.waiter_count.clone(),
        }
    }
}

impl LockManager {
    pub fn new() -> Self {
        let waiter_mgr_worker = FutureWorker::new("waiter-manager");
        let detector_worker = FutureWorker::new("deadlock-detector");

        Self {
            waiter_mgr_scheduler: WaiterMgrScheduler::new(waiter_mgr_worker.scheduler()),
            waiter_mgr_worker: Some(waiter_mgr_worker),
            detector_scheduler: DetectorScheduler::new(detector_worker.scheduler()),
            detector_worker: Some(detector_worker),
            waiter_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Starts `WaiterManager` and `Detector`.
    pub fn start<S: StoreAddrResolver + 'static>(
        &mut self,
        store_id: u64,
        pd_client: Arc<RpcClient>,
        resolver: S,
        security_mgr: Arc<SecurityManager>,
        cfg: &Config,
    ) -> Result<()> {
        self.start_waiter_manager(cfg)?;
        self.start_deadlock_detector(store_id, pd_client, resolver, security_mgr, cfg)?;
        Ok(())
    }

    /// Stops `WaiterManager` and `Detector`.
    pub fn stop(&mut self) {
        self.stop_waiter_manager();
        self.stop_deadlock_detector();
    }

    fn start_waiter_manager(&mut self, cfg: &Config) -> Result<()> {
        let waiter_mgr_runner = WaiterManager::new(
            Arc::clone(&self.waiter_count),
            self.detector_scheduler.clone(),
            cfg,
        );
        self.waiter_mgr_worker
            .as_mut()
            .expect("worker should be some")
            .start(waiter_mgr_runner)?;
        Ok(())
    }

    fn stop_waiter_manager(&mut self) {
        if let Some(Err(e)) = self
            .waiter_mgr_worker
            .take()
            .and_then(|mut w| w.stop())
            .map(JoinHandle::join)
        {
            info!(
                "ignore failure when stopping waiter manager worker";
                "err" => ?e
            );
        }
    }

    fn start_deadlock_detector<S: StoreAddrResolver + 'static>(
        &mut self,
        store_id: u64,
        pd_client: Arc<RpcClient>,
        resolver: S,
        security_mgr: Arc<SecurityManager>,
        cfg: &Config,
    ) -> Result<()> {
        let detector_runner = Detector::new(
            store_id,
            pd_client,
            resolver,
            security_mgr,
            self.waiter_mgr_scheduler.clone(),
            cfg,
        );
        self.detector_worker
            .as_mut()
            .expect("worker should be some")
            .start(detector_runner)?;
        Ok(())
    }

    fn stop_deadlock_detector(&mut self) {
        if let Some(Err(e)) = self
            .detector_worker
            .take()
            .and_then(|mut w| w.stop())
            .map(JoinHandle::join)
        {
            info!(
                "ignore failure when stopping deadlock detector worker";
                "err" => ?e
            );
        }
    }

    /// Creates a `Scheduler` of the deadlock detector worker and registers it to
    /// the `CoprocessorHost` to observe the role change events of the leader region.
    pub fn register_detector_role_change_observer(&self, host: &mut CoprocessorHost) {
        host.registry
            .register_role_observer(1, Box::new(self.detector_scheduler.clone()));
    }

    /// Creates a `DeadlockService` to handle deadlock detect requests from other nodes.
    pub fn deadlock_service(&self) -> DeadlockService {
        DeadlockService::new(
            self.waiter_mgr_scheduler.clone(),
            self.detector_scheduler.clone(),
        )
    }
}

impl LockMgr for LockManager {
    fn wait_for(
        &self,
        start_ts: u64,
        cb: StorageCb,
        pr: ProcessResult,
        lock: Lock,
        is_first_lock: bool,
    ) {
        // Increase `waiter_count` here to prevent there is an on-the-fly WaitFor msg
        // but the waiter_mgr haven't processed it, subsequent WakeUp msgs may be lost.
        self.waiter_count.fetch_add(1, Ordering::SeqCst);
        self.waiter_mgr_scheduler.wait_for(start_ts, cb, pr, lock);

        // If it is the first lock the transaction waits for, it won't cause deadlock.
        if !is_first_lock {
            self.detector_scheduler.detect(start_ts, lock);
        }
    }

    fn wake_up(
        &self,
        lock_ts: u64,
        hashes: Option<Vec<u64>>,
        commit_ts: u64,
        is_pessimistic_txn: bool,
    ) {
        // If `hashes` is some, there may be some waiters waiting for these locks.
        // Try to wake up them.
        if let Some(hashes) = hashes {
            self.waiter_mgr_scheduler
                .wake_up(lock_ts, hashes, commit_ts);
        }
        // If these locks belong to a pessimistic transaction, clean up its wait-for entries
        // in the deadlock detector.
        //
        // TODO: only clean up if the transaction once waited for locks.
        if is_pessimistic_txn {
            self.detector_scheduler.clean_up(lock_ts);
        }
    }

    fn has_waiter(&self) -> bool {
        self.waiter_count.load(Ordering::SeqCst) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_has_waiter() {
        let mut lock_mgr = LockManager::new();
        lock_mgr
            .start_waiter_manager(&Config::default())
            .expect("could not start waiter manager");
        assert!(!lock_mgr.has_waiter());
        let (lock_ts, hash) = (10, 1);
        lock_mgr.wait_for(
            20,
            StorageCb::Boolean(Box::new(|_| ())),
            ProcessResult::Res,
            Lock { ts: lock_ts, hash },
            true,
        );
        // new waiters should be sensed immediately
        assert!(lock_mgr.has_waiter());
        lock_mgr.wake_up(lock_ts, Some(vec![hash]), 15, false);
        thread::sleep(Duration::from_secs(1));
        assert!(!lock_mgr.has_waiter());
        lock_mgr.stop_waiter_manager();
    }

    #[bench]
    fn bench_lock_mgr_clone(b: &mut test::Bencher) {
        let lock_mgr = LockManager::new();
        b.iter(|| {
            test::black_box(lock_mgr.clone());
        })
    }
}
