// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{mpsc::channel, Arc};
use std::thread;
use std::time::Duration;

use grpcio::{ChannelBuilder, Environment};
use kvproto::{kvrpcpb::*, tikvpb::TikvClient};
use test_raftstore::*;
use test_storage::new_raft_engine;
use tikv::server::gc_worker::{GcWorker, GC_MAX_EXECUTING_TASKS};
use tikv::storage;
use tikv_util::{collections::HashMap, HandyRwLock};

#[test]
fn test_gcworker_busy() {
    let snapshot_fp = "raftkv_async_snapshot";
    let (_cluster, engine, ctx) = new_raft_engine(3, "");
    let mut gc_worker = GcWorker::new(engine, None, None, None, Default::default());
    gc_worker.start().unwrap();

    fail::cfg(snapshot_fp, "pause").unwrap();
    let (tx1, rx1) = channel();
    // Schedule `GC_MAX_EXECUTING_TASKS - 1` GC requests.
    for _i in 1..GC_MAX_EXECUTING_TASKS {
        let tx1 = tx1.clone();
        gc_worker
            .gc(
                ctx.clone(),
                1.into(),
                Box::new(move |res: storage::Result<()>| {
                    assert!(res.is_ok());
                    tx1.send(1).unwrap();
                }),
            )
            .unwrap();
    }
    // Sleep to make sure the failpoint is triggered.
    thread::sleep(Duration::from_millis(2000));
    // Schedule one more request. So that there is a request being processed and
    // `GC_MAX_EXECUTING_TASKS` requests in queue.
    gc_worker
        .gc(
            ctx,
            1.into(),
            Box::new(move |res: storage::Result<()>| {
                assert!(res.is_ok());
                tx1.send(1).unwrap();
            }),
        )
        .unwrap();

    // Old GC commands are blocked, the new one will get GcWorkerTooBusy error.
    let (tx2, rx2) = channel();
    gc_worker
        .gc(
            Context::default(),
            1.into(),
            Box::new(move |res: storage::Result<()>| {
                match res {
                    Err(storage::Error(box storage::ErrorInner::GcWorkerTooBusy)) => {}
                    res => panic!("expect too busy, got {:?}", res),
                }
                tx2.send(1).unwrap();
            }),
        )
        .unwrap();

    rx2.recv().unwrap();
    fail::remove(snapshot_fp);
    for _ in 0..GC_MAX_EXECUTING_TASKS {
        rx1.recv().unwrap();
    }
}

// In theory, raft can propose conf change as long as there is no pending one. Replicas
// don't apply logs synchronously, so it's possible the old leader is removed before the new
// leader applies all logs.
// In the current implementation, the new leader rejects conf change until it applies all logs.
// It guarantees the correctness of green GC. This test is to prevent breaking it in the
// future.
#[test]
fn test_collect_lock_from_stale_leader() {
    let mut cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();
    let leader = cluster.leader_of_region(region_id).unwrap();

    // Create clients.
    let env = Arc::new(Environment::new(1));
    let mut clients = HashMap::default();
    for node_id in cluster.get_node_ids() {
        let channel =
            ChannelBuilder::new(Arc::clone(&env)).connect(cluster.sim.rl().get_addr(node_id));
        let client = TikvClient::new(channel);
        clients.insert(node_id, client);
    }

    // Start transferring the region to store 2.
    let new_peer = new_peer(2, 1003);
    cluster.pd_client.must_add_peer(region_id, new_peer.clone());

    // Create the ctx of the first region.
    let leader_client = clients.get(&leader.get_store_id()).unwrap();
    let mut ctx = Context::default();
    ctx.set_region_id(region_id);
    ctx.set_peer(leader.clone());
    ctx.set_region_epoch(cluster.get_region_epoch(region_id));

    // Pause the new peer applying so that when it becomes the leader, it doesn't apply all logs.
    let new_leader_apply_fp = "on_handle_apply_1003";
    fail::cfg(new_leader_apply_fp, "pause").unwrap();
    must_kv_prewrite(
        leader_client,
        ctx,
        vec![new_mutation(Op::Put, b"k1", b"v")],
        b"k1".to_vec(),
        10,
    );

    // Leader election only considers the progress of appending logs, so it can succeed.
    cluster.must_transfer_leader(region_id, new_peer.clone());
    // It shouldn't succeed in the current implementation.
    cluster.pd_client.remove_peer(region_id, leader.clone());
    std::thread::sleep(Duration::from_secs(1));
    cluster.pd_client.must_have_peer(region_id, leader);

    // Must scan the lock from the old leader.
    let locks = must_physical_scan_lock(leader_client, Context::default(), 100, b"", 10);
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].get_key(), b"k1");

    // Can't scan the lock from the new leader.
    let leader_client = clients.get(&new_peer.get_store_id()).unwrap();
    must_register_lock_observer(leader_client, 100);
    let locks = must_check_lock_observer(leader_client, 100, true);
    assert!(locks.is_empty());
    let locks = must_physical_scan_lock(leader_client, Context::default(), 100, b"", 10);
    assert!(locks.is_empty());

    fail::remove(new_leader_apply_fp);
}

#[test]
fn test_observer_send_error() {
    let (_cluster, client, ctx) = must_new_cluster_and_kv_client();

    let max_ts = 100;
    must_register_lock_observer(&client, max_ts);
    must_kv_prewrite(
        &client,
        ctx.clone(),
        vec![new_mutation(Op::Put, b"k1", b"v")],
        b"k1".to_vec(),
        10,
    );
    assert_eq!(must_check_lock_observer(&client, max_ts, true).len(), 1);

    let observer_send_fp = "lock_observer_send";
    fail::cfg(observer_send_fp, "return").unwrap();
    must_kv_prewrite(
        &client,
        ctx,
        vec![new_mutation(Op::Put, b"k2", b"v")],
        b"k1".to_vec(),
        10,
    );
    let resp = check_lock_observer(&client, max_ts);
    assert!(resp.get_error().is_empty(), "{:?}", resp.get_error());
    // Should mark dirty if fails to send locks.
    assert!(!resp.get_is_clean());
}

#[test]
fn test_notify_observer_after_apply() {
    let (mut cluster, client, ctx) = must_new_cluster_and_kv_client();
    cluster.pd_client.disable_default_operator();
    let post_apply_query_fp = "notify_lock_observer_query";
    let apply_plain_kvs_fp = "notify_lock_observer_snapshot";

    // Write a lock and pause before notifying the lock observer.
    let max_ts = 100;
    must_register_lock_observer(&client, max_ts);
    fail::cfg(post_apply_query_fp, "pause").unwrap();
    let key = b"k";
    let (client_clone, ctx_clone) = (client.clone(), ctx.clone());
    std::thread::spawn(move || {
        must_kv_prewrite(
            &client_clone,
            ctx_clone,
            vec![new_mutation(Op::Put, key, b"v")],
            key.to_vec(),
            10,
        );
    });
    // We can use physical_scan_lock to get the lock because we notify the lock observer after writing data to the rocskdb.
    let mut locks = vec![];
    for _ in 1..100 {
        sleep_ms(10);
        assert!(must_check_lock_observer(&client, max_ts, true).is_empty());
        locks.extend(must_physical_scan_lock(
            &client,
            ctx.clone(),
            max_ts,
            b"",
            100,
        ));
        if !locks.is_empty() {
            break;
        }
    }
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].get_key(), key);
    fail::remove(post_apply_query_fp);
    assert_eq!(must_check_lock_observer(&client, max_ts, true).len(), 1);

    // Add a new store.
    let store_id = cluster.add_new_engine();
    let channel = ChannelBuilder::new(Arc::new(Environment::new(1)))
        .connect(cluster.sim.rl().get_addr(store_id));
    let replica_client = TikvClient::new(channel);

    // Add a new peer and pause before notifying the lock observer.
    must_register_lock_observer(&replica_client, max_ts);
    fail::cfg(apply_plain_kvs_fp, "pause").unwrap();
    cluster
        .pd_client
        .must_add_peer(ctx.get_region_id(), new_peer(store_id, store_id));
    // We can use physical_scan_lock to get the lock because we notify the lock observer after writing data to the rocskdb.
    let mut locks = vec![];
    for _ in 1..100 {
        sleep_ms(10);
        assert!(must_check_lock_observer(&replica_client, max_ts, true).is_empty());
        locks.extend(must_physical_scan_lock(
            &replica_client,
            ctx.clone(),
            max_ts,
            b"",
            100,
        ));
        if !locks.is_empty() {
            break;
        }
    }
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].get_key(), key);
    fail::remove(apply_plain_kvs_fp);
    assert_eq!(
        must_check_lock_observer(&replica_client, max_ts, true).len(),
        1
    );
}

// It may cause locks missing during green GC if the raftstore notifies the lock observer before writing data to the rocksdb:
//   1. Store-1 transfers a region to store-2 and store-2 is applying logs.
//   2. GC worker registers lock observer on store-2 after calling lock observer's callback and before finishing applying which means the lock won't be observed.
//   3. GC worker scans locks on each store independently. It's possible GC worker has scanned all locks on store-2 and hasn't scanned locks on store-1.
//   4. Store-2 applies all logs and removes the peer on store-1.
//   5. GC worker can't scan the lock on store-1 because the peer has been destroyed.
//   6. GC worker can't get the lock from store-2 because it can't observe the lock and has scanned it.
#[test]
fn test_collect_applying_locks() {
    let mut cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let region_id = cluster.run_conf_change();
    let leader = cluster.leader_of_region(region_id).unwrap();

    // Create clients.
    let env = Arc::new(Environment::new(1));
    let mut clients = HashMap::default();
    for node_id in cluster.get_node_ids() {
        let channel =
            ChannelBuilder::new(Arc::clone(&env)).connect(cluster.sim.rl().get_addr(node_id));
        let client = TikvClient::new(channel);
        clients.insert(node_id, client);
    }

    // Start transferring the region to store 2.
    let new_peer = new_peer(2, 1003);
    cluster.pd_client.must_add_peer(region_id, new_peer.clone());

    // Create the ctx of the first region.
    let store_1_client = clients.get(&leader.get_store_id()).unwrap();
    let mut ctx = Context::default();
    ctx.set_region_id(region_id);
    ctx.set_peer(leader.clone());
    ctx.set_region_epoch(cluster.get_region_epoch(region_id));

    // Pause store-2 after calling observer callbacks and before writing to the rocksdb.
    let new_leader_apply_fp = "post_handle_apply_1003";
    fail::cfg(new_leader_apply_fp, "pause").unwrap();

    // Write 1 lock.
    must_kv_prewrite(
        &store_1_client,
        ctx,
        vec![new_mutation(Op::Put, b"k1", b"v")],
        b"k1".to_vec(),
        10,
    );
    // Wait for store-2 applying.
    std::thread::sleep(Duration::from_secs(3));

    // Starting the process of green GC at safe point 20:
    //   1. Register lock observers on all stores.
    //   2. Scan locks physically on each store independently.
    //   3. Get locks from all observers.
    let safe_point = 20;

    // Register lock observers.
    clients.iter().for_each(|(_, c)| {
        must_register_lock_observer(c, safe_point);
    });

    // Finish scanning locks on store-2 and find nothing.
    let store_2_client = clients.get(&new_peer.get_store_id()).unwrap();
    let locks = must_physical_scan_lock(store_2_client, Context::default(), safe_point, b"", 1);
    assert!(locks.is_empty(), "{:?}", locks);

    // Transfer the region from store-1 to store-2.
    fail::remove(new_leader_apply_fp);
    cluster.must_transfer_leader(region_id, new_peer);
    cluster.pd_client.must_remove_peer(region_id, leader);
    // Wait for store-1 desroying the region.
    std::thread::sleep(Duration::from_secs(3));

    // Scan locks on store-1 after the region has been destroyed.
    let locks = must_physical_scan_lock(store_1_client, Context::default(), safe_point, b"", 1);
    assert!(locks.is_empty(), "{:?}", locks);

    // Check lock observers.
    let mut locks = vec![];
    clients.iter().for_each(|(_, c)| {
        locks.extend(must_check_lock_observer(c, safe_point, true));
    });
    // Must observe the applying lock even through we can't use scan to get it.
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].get_key(), b"k1");
}
