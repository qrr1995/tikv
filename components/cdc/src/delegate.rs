// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::mem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[cfg(not(feature = "prost-codec"))]
use kvproto::cdcpb::*;
#[cfg(feature = "prost-codec")]
use kvproto::cdcpb::{
    event::{
        row::OpType as EventRowOpType, Entries as EventEntries, Error as EventError,
        Event as Event_oneof_event, LogType as EventLogType, Row as EventRow,
    },
    ChangeDataEvent, Event,
};

use futures::sync::mpsc::*;
use kvproto::metapb::{Region, RegionEpoch};
use kvproto::raft_cmdpb::{AdminCmdType, AdminRequest, AdminResponse, CmdType, Request};
use resolved_ts::Resolver;
use tikv::raftstore::store::util::compare_region_epoch;
use tikv::raftstore::Error as RaftStoreError;
use tikv::storage::mvcc::{Lock, LockType, WriteRef, WriteType};
use tikv::storage::txn::TxnEntry;
use tikv_util::collections::HashMap;
use txn_types::{Key, TimeStamp};

use crate::Error;

static DOWNSTREAM_ID_ALLOC: AtomicUsize = AtomicUsize::new(0);

/// A unique identifier of a Downstream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownstreamID(usize);

impl DownstreamID {
    pub fn new() -> DownstreamID {
        DownstreamID(DOWNSTREAM_ID_ALLOC.fetch_add(1, Ordering::SeqCst))
    }
}

#[derive(Clone)]
pub struct Downstream {
    // TODO: include cdc request.
    /// A unique identifier of the Downstream.
    pub id: DownstreamID,
    // The IP address of downstream.
    peer: String,
    region_epoch: RegionEpoch,
    sink: UnboundedSender<ChangeDataEvent>,
}

impl Downstream {
    /// Create a Downsteam.
    ///
    /// peer is the address of the downstream.
    /// sink sends data to the downstream.
    pub fn new(
        peer: String,
        region_epoch: RegionEpoch,
        sink: UnboundedSender<ChangeDataEvent>,
    ) -> Downstream {
        Downstream {
            id: DownstreamID::new(),
            peer,
            sink,
            region_epoch,
        }
    }

    fn sink(&self, change_data: ChangeDataEvent) {
        if self.sink.unbounded_send(change_data).is_err() {
            error!("send event failed"; "downstream" => %self.peer);
        }
    }
}

#[derive(Default)]
struct Pending {
    // Batch of RaftCommand observed from raftstore
    // TODO add multi_batch once CDC observer is ready
    multi_batch: (),
    downstreams: Vec<Downstream>,
    scan: Vec<(DownstreamID, Vec<Option<TxnEntry>>)>,
}

/// A CDC delegate of a raftstore region peer.
///
/// It converts raft commands into CDC events and broadcast to downstreams.
/// It also track trancation on the fly in order to compute resolved ts.
pub struct Delegate {
    pub region_id: u64,
    region: Option<Region>,
    pub downstreams: Vec<Downstream>,
    pub resolver: Option<Resolver>,
    pending: Option<Pending>,
    enabled: Arc<AtomicBool>,
    failed: bool,
}

impl Delegate {
    /// Create a Delegate the given region.
    pub fn new(region_id: u64) -> Delegate {
        Delegate {
            region_id,
            downstreams: Vec::new(),
            resolver: None,
            region: None,
            pending: Some(Pending::default()),
            enabled: Arc::new(AtomicBool::new(true)),
            failed: false,
        }
    }

    /// Returns a shared flag.
    /// True if there are some active downstreams subscribe the region.
    /// False if all downstreams has unsubscribed.
    pub fn enabled(&self) -> Arc<AtomicBool> {
        self.enabled.clone()
    }

    pub fn subscribe(&mut self, downstream: Downstream) {
        if let Some(region) = self.region.as_ref() {
            if let Err(e) = compare_region_epoch(
                &downstream.region_epoch,
                region,
                false, /* check_conf_ver */
                true,  /* check_ver */
                true,  /* include_region */
            ) {
                let err = Error::Request(e.into());
                let change_data_error = self.error_event(err);
                downstream.sink(change_data_error);
                return;
            }
            self.downstreams.push(downstream);
        } else {
            self.pending.as_mut().unwrap().downstreams.push(downstream);
        }
    }

    pub fn unsubscribe(&mut self, id: DownstreamID, err: Option<Error>) -> bool {
        let change_data_error = err.map(|err| self.error_event(err));
        let downstreams = if self.pending.is_some() {
            &mut self.pending.as_mut().unwrap().downstreams
        } else {
            &mut self.downstreams
        };
        downstreams.retain(|d| {
            if d.id == id {
                if let Some(change_data_error) = change_data_error.clone() {
                    d.sink(change_data_error);
                }
            }
            d.id != id
        });
        let is_last = self.downstreams.is_empty();
        if is_last {
            self.enabled.store(false, Ordering::SeqCst);
        }
        is_last
    }

    fn error_event(&self, err: Error) -> ChangeDataEvent {
        let mut change_data_event = Event::default();
        let mut cdc_err = EventError::default();
        let mut err = err.extract_error_header();
        if err.has_region_not_found() {
            let region_not_found = err.take_region_not_found();
            cdc_err.set_region_not_found(region_not_found);
        } else if err.has_not_leader() {
            let not_leader = err.take_not_leader();
            cdc_err.set_not_leader(not_leader);
        } else if err.has_epoch_not_match() {
            let epoch_not_match = err.take_epoch_not_match();
            cdc_err.set_epoch_not_match(epoch_not_match);
        } else {
            panic!(
                "region met unknown error region_id: {}, error: {:?}",
                self.region_id, err
            );
        }
        change_data_event.event = Some(Event_oneof_event::Error(cdc_err));
        change_data_event.region_id = self.region_id;
        let mut change_data = ChangeDataEvent::default();
        change_data.mut_events().push(change_data_event);
        change_data
    }

    /// Fail the delegate
    ///
    /// This means the region has met an unrecoverable error for CDC.
    /// It broadcasts errors to all downstream and stops.
    pub fn fail(&mut self, err: Error) {
        // Stop observe further events.
        self.enabled.store(false, Ordering::SeqCst);

        info!("region met error";
            "region_id" => self.region_id, "error" => ?err);
        let change_data = self.error_event(err);
        self.broadcast(change_data);

        // Mark this delegate has failed.
        self.failed = true;
    }

    pub fn has_failed(&self) -> bool {
        self.failed
    }

    fn broadcast(&self, change_data: ChangeDataEvent) {
        let downstreams = if self.pending.is_some() {
            &self.pending.as_ref().unwrap().downstreams
        } else {
            &self.downstreams
        };
        for d in downstreams {
            d.sink(change_data.clone());
        }
    }

    /// Install a resolver and notify downstreams this region if ready to serve.
    pub fn on_region_ready(&mut self, resolver: Resolver, region: Region) {
        assert!(
            self.resolver.is_none(),
            "region resolver should not be ready"
        );
        self.resolver = Some(resolver);
        self.region = Some(region);
        if let Some(pending) = self.pending.take() {
            // Re-subscribe pending downstreams.
            for downstream in pending.downstreams {
                self.subscribe(downstream);
            }
            for (downstream_id, entries) in pending.scan {
                self.on_scan(downstream_id, entries);
            }
            // TODO iter multi_batch once CDC observer is ready.
            // for batch in pending.multi_batch {
            //     self.on_batch(batch);
            // }
        }
        info!("region is ready"; "region_id" => self.region_id);
    }

    /// Try advance and broadcast resolved ts.
    pub fn on_min_ts(&mut self, min_ts: TimeStamp) {
        if self.resolver.is_none() {
            info!("region resolver not ready";
                "region_id" => self.region_id, "min_ts" => min_ts);
            return;
        }
        info!("try to advance ts"; "region_id" => self.region_id);
        let resolver = self.resolver.as_mut().unwrap();
        let resolved_ts = match resolver.resolve(min_ts) {
            Some(rts) => rts,
            None => return,
        };
        info!("resolved ts updated";
            "region_id" => self.region_id, "resolved_ts" => resolved_ts);
        let mut change_data_event = Event::default();
        change_data_event.region_id = self.region_id;
        change_data_event.event = Some(Event_oneof_event::ResolvedTs(resolved_ts.into_inner()));
        let mut change_data = ChangeDataEvent::default();
        change_data.mut_events().push(change_data_event);
        self.broadcast(change_data);
    }

    // TODO fill on_batch when CDC observer is ready.
    pub fn on_batch(&mut self, _batch: () /* CmdBatch */) {
        unimplemented!()
    }

    pub fn on_scan(&mut self, downstream_id: DownstreamID, entries: Vec<Option<TxnEntry>>) {
        if let Some(pending) = self.pending.as_mut() {
            pending.scan.push((downstream_id, entries));
            return;
        }
        let d = if let Some(d) = self.downstreams.iter_mut().find(|d| d.id == downstream_id) {
            d
        } else {
            warn!("downstream not found"; "downstream_id" => ?downstream_id);
            return;
        };

        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                Some(TxnEntry::Prewrite { default, lock }) => {
                    let mut row = EventRow::default();
                    let skip = decode_lock(lock.0, &lock.1, &mut row);
                    if skip {
                        continue;
                    }
                    decode_default(default.1, &mut row);
                    rows.push(row);
                }
                Some(TxnEntry::Commit { default, write }) => {
                    let mut row = EventRow::default();
                    let skip = decode_write(write.0, &write.1, &mut row);
                    if skip {
                        continue;
                    }
                    decode_default(default.1, &mut row);

                    // This type means the row is self-contained, it has,
                    //   1. start_ts
                    //   2. commit_ts
                    //   3. key
                    //   4. value
                    if row.get_type() == EventLogType::Rollback {
                        // We dont need to send rollbacks to downstream,
                        // because downstream does not needs rollback to clean
                        // prewrite as it drops all previous stashed data.
                        continue;
                    }
                    set_event_row_type(&mut row, EventLogType::Committed);
                    rows.push(row);
                }
                None => {
                    let mut row = EventRow::default();

                    // This type means scan has finised.
                    set_event_row_type(&mut row, EventLogType::Initialized);
                    rows.push(row);
                }
            }
        }

        let mut event_entries = EventEntries::default();
        event_entries.entries = rows.into();
        let mut change_data_event = Event::default();
        change_data_event.region_id = self.region_id;
        change_data_event.event = Some(Event_oneof_event::Entries(event_entries));
        let mut change_data = ChangeDataEvent::default();
        change_data.mut_events().push(change_data_event);
        d.sink(change_data);
    }

    fn sink_data(&mut self, index: u64, requests: Vec<Request>) {
        let mut rows = HashMap::default();
        for mut req in requests {
            // CDC cares about put requests only.
            if req.get_cmd_type() != CmdType::Put {
                // Do not log delete requests because they are issued by GC
                // frequently.
                if req.get_cmd_type() != CmdType::Delete {
                    debug!(
                        "skip other command";
                        "region_id" => self.region_id,
                        "command" => ?req,
                    );
                }
                continue;
            }
            let mut put = req.take_put();
            match put.cf.as_str() {
                "write" => {
                    let mut row = EventRow::default();
                    let skip = decode_write(put.take_key(), put.get_value(), &mut row);
                    if skip {
                        continue;
                    }

                    // In order to advance resolved ts,
                    // we must untrack inflight txns if they are committed.
                    assert!(self.resolver.is_some(), "region resolver should be ready");
                    let resolver = self.resolver.as_mut().unwrap();
                    let commit_ts = if row.commit_ts == 0 {
                        None
                    } else {
                        Some(row.commit_ts)
                    };
                    resolver.untrack_lock(
                        row.start_ts.into(),
                        commit_ts.map(Into::into),
                        row.key.clone(),
                    );

                    let r = rows.insert(row.key.clone(), row);
                    assert!(r.is_none());
                }
                "lock" => {
                    let mut row = EventRow::default();
                    let skip = decode_lock(put.take_key(), put.get_value(), &mut row);
                    if skip {
                        continue;
                    }

                    let occupied = rows.entry(row.key.clone()).or_default();
                    if !occupied.value.is_empty() {
                        assert!(row.value.is_empty());
                        let mut value = vec![];
                        mem::swap(&mut occupied.value, &mut value);
                        row.value = value;
                    }

                    // In order to compute resolved ts,
                    // we must track inflight txns.
                    assert!(self.resolver.is_some(), "region resolver should be ready");
                    let resolver = self.resolver.as_mut().unwrap();
                    resolver.track_lock(row.start_ts.into(), row.key.clone());

                    *occupied = row;
                }
                "" | "default" => {
                    let key = Key::from_encoded(put.take_key()).truncate_ts().unwrap();
                    let row = rows.entry(key.to_raw().unwrap()).or_default();
                    decode_default(put.take_value(), row);
                }
                other => {
                    panic!("invalid cf {}", other);
                }
            }
        }
        let mut entries = Vec::with_capacity(rows.len());
        for (_, v) in rows {
            entries.push(v);
        }
        let mut event_entries = EventEntries::default();
        event_entries.entries = entries.into();
        let mut change_data_event = Event::default();
        change_data_event.region_id = self.region_id;
        change_data_event.index = index;
        change_data_event.event = Some(Event_oneof_event::Entries(event_entries));
        let mut change_data = ChangeDataEvent::default();
        change_data.mut_events().push(change_data_event);
        self.broadcast(change_data);
    }

    fn sink_admin(&mut self, request: AdminRequest, mut response: AdminResponse) {
        let store_err = match request.get_cmd_type() {
            AdminCmdType::Split => RaftStoreError::EpochNotMatch(
                "split".to_owned(),
                vec![
                    response.mut_split().take_left(),
                    response.mut_split().take_right(),
                ],
            ),
            AdminCmdType::BatchSplit => RaftStoreError::EpochNotMatch(
                "batchsplit".to_owned(),
                response.mut_splits().take_regions().into(),
            ),
            AdminCmdType::PrepareMerge
            | AdminCmdType::CommitMerge
            | AdminCmdType::RollbackMerge => {
                RaftStoreError::EpochNotMatch("merge".to_owned(), vec![])
            }
            _ => return,
        };
        let err = Error::Request(store_err.into());
        self.fail(err);
    }
}

fn set_event_row_type(row: &mut EventRow, ty: EventLogType) {
    #[cfg(feature = "prost-codec")]
    {
        row.r#type = ty.into();
    }
    #[cfg(not(feature = "prost-codec"))]
    {
        row.r_type = ty;
    }
}

fn decode_write(key: Vec<u8>, value: &[u8], row: &mut EventRow) -> bool {
    let write = WriteRef::parse(value).unwrap().to_owned();
    let (op_type, r_type) = match write.write_type {
        WriteType::Put => (EventRowOpType::Put, EventLogType::Commit),
        WriteType::Delete => (EventRowOpType::Delete, EventLogType::Commit),
        WriteType::Rollback => (EventRowOpType::Unknown, EventLogType::Rollback),
        other => {
            debug!("skip write record"; "write" => ?other);
            return true;
        }
    };
    let key = Key::from_encoded(key);
    let commit_ts = if write.write_type == WriteType::Rollback {
        0
    } else {
        key.decode_ts().unwrap().into_inner()
    };
    row.start_ts = write.start_ts.into_inner();
    row.commit_ts = commit_ts;
    row.key = key.truncate_ts().unwrap().to_raw().unwrap();
    row.op_type = op_type.into();
    set_event_row_type(row, r_type);
    if let Some(value) = write.short_value {
        row.value = value;
    }

    false
}

fn decode_lock(key: Vec<u8>, value: &[u8], row: &mut EventRow) -> bool {
    let lock = Lock::parse(value).unwrap();
    let op_type = match lock.lock_type {
        LockType::Put => EventRowOpType::Put,
        LockType::Delete => EventRowOpType::Delete,
        other => {
            info!("skip lock record";
                "type" => ?other,
                "start_ts" => ?lock.ts,
                "for_update_ts" => ?lock.for_update_ts);
            return true;
        }
    };
    let key = Key::from_encoded(key);
    row.start_ts = lock.ts.into_inner();
    row.key = key.to_raw().unwrap();
    row.op_type = op_type.into();
    set_event_row_type(row, EventLogType::Prewrite);
    if let Some(value) = lock.short_value {
        row.value = value;
    }

    false
}

fn decode_default(value: Vec<u8>, row: &mut EventRow) {
    if !value.is_empty() {
        row.value = value.to_vec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::rocks::*;
    use engine_rocks::{RocksEngine, RocksSnapshot};
    use engine_traits::Snapshot;
    use futures::{Future, Stream};
    use kvproto::errorpb::Error as ErrorHeader;
    use kvproto::metapb::Region;
    use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse, Response};
    use kvproto::raft_serverpb::RaftMessage;
    use std::cell::Cell;
    use std::sync::Arc;
    use tikv::raftstore::router::RaftStoreRouter;
    use tikv::raftstore::store::{
        Callback, CasualMessage, ReadResponse, RegionSnapshot, SignificantMsg,
    };
    use tikv::raftstore::Result as RaftStoreResult;
    use tikv::server::RaftKv;
    use tikv::storage::mvcc::test_util::*;
    use tikv::storage::mvcc::tests::*;
    use tikv_util::mpsc::{bounded, Sender as UtilSender};

    // TODO add test_txn once cdc observer is ready.
    // https://github.com/overvenus/tikv/blob/447d10ae80b5b7fc58a4bef4631874a11237fdcf/components/cdc/src/delegate.rs#L615-L701

    #[test]
    fn test_error() {
        let region_id = 1;
        let mut region = Region::default();
        region.set_id(region_id);
        region.mut_peers().push(Default::default());
        region.mut_region_epoch().set_version(2);
        region.mut_region_epoch().set_conf_ver(2);
        let region_epoch = region.get_region_epoch().clone();

        let (sink, events) = unbounded();
        let mut delegate = Delegate::new(region_id);
        delegate.subscribe(Downstream::new(String::new(), region_epoch, sink));
        let enabled = delegate.enabled();
        assert!(enabled.load(Ordering::SeqCst));
        let mut resolver = Resolver::new();
        resolver.init();
        delegate.on_region_ready(resolver, region);

        let events_wrap = Cell::new(Some(events));
        let receive_error = || {
            let (change_data, events) = events_wrap
                .replace(None)
                .unwrap()
                .into_future()
                .wait()
                .unwrap();
            events_wrap.set(Some(events));
            let mut change_data = change_data.unwrap();
            assert_eq!(change_data.events.len(), 1);
            let change_data_event = &mut change_data.events[0];
            let event = change_data_event.event.take().unwrap();
            match event {
                Event_oneof_event::Error(err) => err,
                _ => panic!("unknown event"),
            }
        };

        let mut err_header = ErrorHeader::default();
        err_header.set_not_leader(Default::default());
        delegate.fail(Error::Request(err_header));
        let err = receive_error();
        assert!(err.has_not_leader());
        // Enable is disabled by any error.
        assert!(!enabled.load(Ordering::SeqCst));

        let mut err_header = ErrorHeader::default();
        err_header.set_region_not_found(Default::default());
        delegate.fail(Error::Request(err_header));
        let err = receive_error();
        assert!(err.has_region_not_found());

        let mut err_header = ErrorHeader::default();
        err_header.set_epoch_not_match(Default::default());
        delegate.fail(Error::Request(err_header));
        let err = receive_error();
        assert!(err.has_epoch_not_match());

        // Split
        let mut region = Region::default();
        region.set_id(1);
        let mut request = AdminRequest::default();
        request.set_cmd_type(AdminCmdType::Split);
        let mut response = AdminResponse::default();
        response.mut_split().set_left(region.clone());
        delegate.sink_admin(request, response);
        let mut err = receive_error();
        assert!(err.has_epoch_not_match());
        err.take_epoch_not_match()
            .current_regions
            .into_iter()
            .find(|r| r.get_id() == 1)
            .unwrap();

        let mut request = AdminRequest::default();
        request.set_cmd_type(AdminCmdType::BatchSplit);
        let mut response = AdminResponse::default();
        response.mut_splits().set_regions(vec![region].into());
        delegate.sink_admin(request, response);
        let mut err = receive_error();
        assert!(err.has_epoch_not_match());
        err.take_epoch_not_match()
            .current_regions
            .into_iter()
            .find(|r| r.get_id() == 1)
            .unwrap();

        // Merge
        let mut request = AdminRequest::default();
        request.set_cmd_type(AdminCmdType::PrepareMerge);
        let response = AdminResponse::default();
        delegate.sink_admin(request, response);
        let mut err = receive_error();
        assert!(err.has_epoch_not_match());
        assert!(err.take_epoch_not_match().current_regions.is_empty());

        let mut request = AdminRequest::default();
        request.set_cmd_type(AdminCmdType::CommitMerge);
        let response = AdminResponse::default();
        delegate.sink_admin(request, response);
        let mut err = receive_error();
        assert!(err.has_epoch_not_match());
        assert!(err.take_epoch_not_match().current_regions.is_empty());

        let mut request = AdminRequest::default();
        request.set_cmd_type(AdminCmdType::RollbackMerge);
        let response = AdminResponse::default();
        delegate.sink_admin(request, response);
        let mut err = receive_error();
        assert!(err.has_epoch_not_match());
        assert!(err.take_epoch_not_match().current_regions.is_empty());
    }

    #[test]
    fn test_scan() {
        let region_id = 1;
        let mut region = Region::default();
        region.set_id(region_id);
        region.mut_peers().push(Default::default());
        region.mut_region_epoch().set_version(2);
        region.mut_region_epoch().set_conf_ver(2);
        let region_epoch = region.get_region_epoch().clone();

        let (sink, events) = unbounded();
        let mut delegate = Delegate::new(region_id);
        let downstream = Downstream::new(String::new(), region_epoch, sink);
        let downstream_id = downstream.id;
        delegate.subscribe(downstream);
        let enabled = delegate.enabled();
        assert!(enabled.load(Ordering::SeqCst));

        let events_wrap = Cell::new(Some(events));
        let check_event = |event_rows: Vec<EventRow>| {
            let (change_data, events) = events_wrap
                .replace(None)
                .unwrap()
                .into_future()
                .wait()
                .unwrap();
            events_wrap.set(Some(events));
            let mut change_data = change_data.unwrap();
            assert_eq!(change_data.events.len(), 1);
            let change_data_event = &mut change_data.events[0];
            assert_eq!(change_data_event.region_id, region_id);
            assert_eq!(change_data_event.index, 0);
            let event = change_data_event.event.take().unwrap();
            match event {
                Event_oneof_event::Entries(entries) => {
                    assert_eq!(entries.entries.as_slice(), event_rows.as_slice());
                }
                _ => panic!("unknown event"),
            }
        };

        // Stashed in pending before region ready.
        let entries = vec![
            Some(
                EntryBuilder {
                    key: b"a".to_vec(),
                    value: b"b".to_vec(),
                    start_ts: 1.into(),
                    commit_ts: 0.into(),
                    primary: vec![],
                    for_update_ts: 0.into(),
                }
                .build_prewrite(LockType::Put, false),
            ),
            Some(
                EntryBuilder {
                    key: b"a".to_vec(),
                    value: b"b".to_vec(),
                    start_ts: 1.into(),
                    commit_ts: 2.into(),
                    primary: vec![],
                    for_update_ts: 0.into(),
                }
                .build_commit(WriteType::Put, false),
            ),
            Some(
                EntryBuilder {
                    key: b"a".to_vec(),
                    value: b"b".to_vec(),
                    start_ts: 3.into(),
                    commit_ts: 0.into(),
                    primary: vec![],
                    for_update_ts: 0.into(),
                }
                .build_rollback(),
            ),
            None,
        ];
        delegate.on_scan(downstream_id, entries);
        assert_eq!(delegate.pending.as_ref().unwrap().scan.len(), 1);

        let mut resolver = Resolver::new();
        resolver.init();
        delegate.on_region_ready(resolver, region);

        // Flush all pending entries.
        let mut row1 = EventRow::default();
        row1.start_ts = 1;
        row1.commit_ts = 0;
        row1.key = b"a".to_vec();
        row1.op_type = EventRowOpType::Put.into();
        set_event_row_type(&mut row1, EventLogType::Prewrite);
        row1.value = b"b".to_vec();
        let mut row2 = EventRow::default();
        row2.start_ts = 1;
        row2.commit_ts = 2;
        row2.key = b"a".to_vec();
        row2.op_type = EventRowOpType::Put.into();
        set_event_row_type(&mut row2, EventLogType::Committed);
        row2.value = b"b".to_vec();
        let mut row3 = EventRow::default();
        set_event_row_type(&mut row3, EventLogType::Initialized);
        check_event(vec![row1, row2, row3]);
    }
}
