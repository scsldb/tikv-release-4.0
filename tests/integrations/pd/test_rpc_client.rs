// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use futures::Future;
use futures_cpupool::Builder;
use grpcio::EnvBuilder;
use kvproto::metapb;
use kvproto::pdpb;

use pd_client::{validate_endpoints, Error as PdError, PdClient, RegionStat, RpcClient};
use raftstore::store;
use security::{SecurityConfig, SecurityManager};
use tikv_util::config::ReadableDuration;
use txn_types::TimeStamp;

use test_pd::{mocker::*, util::*, Server as MockServer};

#[test]
fn test_retry_rpc_client() {
    let eps_count = 1;
    let mut server = MockServer::new(eps_count);
    let eps = server.bind_addrs();
    let m_eps = eps.clone();
    let mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());
    let m_mgr = mgr.clone();
    server.stop();
    let child = thread::spawn(move || {
        let cfg = new_config(m_eps);
        assert_eq!(RpcClient::new(&cfg, m_mgr).is_ok(), true);
    });
    thread::sleep(Duration::from_millis(500));
    server.start(&mgr, eps);
    assert_eq!(child.join().is_ok(), true);
}

#[test]
fn test_rpc_client() {
    let eps_count = 1;
    let server = MockServer::new(eps_count);
    let eps = server.bind_addrs();

    let client = new_client(eps.clone(), None);
    assert_ne!(client.get_cluster_id().unwrap(), 0);

    let store_id = client.alloc_id().unwrap();
    let mut store = metapb::Store::default();
    store.set_id(store_id);
    debug!("bootstrap store {:?}", store);

    let peer_id = client.alloc_id().unwrap();
    let mut peer = metapb::Peer::default();
    peer.set_id(peer_id);
    peer.set_store_id(store_id);

    let region_id = client.alloc_id().unwrap();
    let mut region = metapb::Region::default();
    region.set_id(region_id);
    region.mut_peers().push(peer.clone());
    debug!("bootstrap region {:?}", region);

    client
        .bootstrap_cluster(store.clone(), region.clone())
        .unwrap();
    assert_eq!(client.is_cluster_bootstrapped().unwrap(), true);

    let tmp_stores = client.get_all_stores(false).unwrap();
    assert_eq!(tmp_stores.len(), 1);
    assert_eq!(tmp_stores[0], store);

    let tmp_store = client.get_store(store_id).unwrap();
    assert_eq!(tmp_store.get_id(), store.get_id());

    let region_key = region.get_start_key();
    let tmp_region = client.get_region(region_key).unwrap();
    assert_eq!(tmp_region.get_id(), region.get_id());

    let region_info = client.get_region_info(region_key).unwrap();
    assert_eq!(region_info.region, region);
    assert_eq!(region_info.leader, None);

    let tmp_region = client.get_region_by_id(region_id).wait().unwrap().unwrap();
    assert_eq!(tmp_region.get_id(), region.get_id());

    let ts = client.get_tso().wait().unwrap();
    assert_ne!(ts, TimeStamp::zero());

    let mut prev_id = 0;
    for _ in 0..100 {
        let client = new_client(eps.clone(), None);
        let alloc_id = client.alloc_id().unwrap();
        assert!(alloc_id > prev_id);
        prev_id = alloc_id;
    }

    let poller = Builder::new()
        .pool_size(1)
        .name_prefix(thd_name!("poller"))
        .create();
    let (tx, rx) = mpsc::channel();
    let f = client.handle_region_heartbeat_response(1, move |resp| {
        let _ = tx.send(resp);
    });
    poller.spawn(f).forget();
    poller
        .spawn(client.region_heartbeat(
            store::RAFT_INIT_LOG_TERM,
            region.clone(),
            peer.clone(),
            RegionStat::default(),
        ))
        .forget();
    rx.recv_timeout(Duration::from_secs(3)).unwrap();

    let region_info = client.get_region_info(region_key).unwrap();
    assert_eq!(region_info.region, region);
    assert_eq!(region_info.leader.unwrap(), peer);

    client
        .store_heartbeat(pdpb::StoreStats::default())
        .wait()
        .unwrap();
    client
        .ask_batch_split(metapb::Region::default(), 1)
        .wait()
        .unwrap();
    client
        .report_batch_split(vec![metapb::Region::default(), metapb::Region::default()])
        .wait()
        .unwrap();

    let region_info = client.get_region_info(region_key).unwrap();
    client.scatter_region(region_info).unwrap();
}

#[test]
fn test_get_tombstone_stores() {
    let eps_count = 1;
    let server = MockServer::new(eps_count);
    let eps = server.bind_addrs();
    let client = new_client(eps, None);

    let mut all_stores = vec![];
    let store_id = client.alloc_id().unwrap();
    let mut store = metapb::Store::default();
    store.set_id(store_id);
    let region_id = client.alloc_id().unwrap();
    let mut region = metapb::Region::default();
    region.set_id(region_id);
    client.bootstrap_cluster(store.clone(), region).unwrap();

    all_stores.push(store);
    assert_eq!(client.is_cluster_bootstrapped().unwrap(), true);
    let s = client.get_all_stores(false).unwrap();
    assert_eq!(s, all_stores);

    // Add tombstone store.
    let mut store99 = metapb::Store::default();
    store99.set_id(99);
    store99.set_state(metapb::StoreState::Tombstone);
    server.default_handler().add_store(store99.clone());

    // do not include tombstone.
    let s = client.get_all_stores(true).unwrap();
    assert_eq!(s, all_stores);

    all_stores.push(store99.clone());
    all_stores.sort_by(|a, b| a.get_id().cmp(&b.get_id()));
    // include tombstone, there should be 2 stores.
    let mut s = client.get_all_stores(false).unwrap();
    s.sort_by(|a, b| a.get_id().cmp(&b.get_id()));
    assert_eq!(s, all_stores);

    // Add another tombstone store.
    let mut store199 = store99;
    store199.set_id(199);
    server.default_handler().add_store(store199.clone());

    all_stores.push(store199);
    all_stores.sort_by(|a, b| a.get_id().cmp(&b.get_id()));
    let mut s = client.get_all_stores(false).unwrap();
    s.sort_by(|a, b| a.get_id().cmp(&b.get_id()));
    assert_eq!(s, all_stores);

    client.get_store(store_id).unwrap();
    client.get_store(99).unwrap_err();
    client.get_store(199).unwrap_err();
}

#[test]
fn test_reboot() {
    let eps_count = 1;
    let server = MockServer::with_case(eps_count, Arc::new(AlreadyBootstrapped));
    let eps = server.bind_addrs();
    let client = new_client(eps, None);

    assert!(!client.is_cluster_bootstrapped().unwrap());

    match client.bootstrap_cluster(metapb::Store::default(), metapb::Region::default()) {
        Err(PdError::ClusterBootstrapped(_)) => (),
        _ => {
            panic!("failed, should return ClusterBootstrapped");
        }
    }
}

#[test]
fn test_validate_endpoints() {
    let eps_count = 3;
    let server = MockServer::with_case(eps_count, Arc::new(Split::new()));
    let env = Arc::new(
        EnvBuilder::new()
            .cq_count(1)
            .name_prefix(thd_name!("test-pd"))
            .build(),
    );
    let eps = server.bind_addrs();

    let mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());
    assert!(validate_endpoints(env, &new_config(eps), mgr.clone()).is_err());
}

fn test_retry<F: Fn(&RpcClient)>(func: F) {
    let eps_count = 1;
    // Retry mocker returns `Err(_)` for most request, here two thirds are `Err(_)`.
    let retry = Arc::new(Retry::new(3));
    let server = MockServer::with_case(eps_count, retry);
    let eps = server.bind_addrs();

    let client = new_client(eps, None);

    for _ in 0..3 {
        func(&client);
    }
}

#[test]
fn test_retry_async() {
    let r#async = |client: &RpcClient| {
        let region = client.get_region_by_id(1);
        region.wait().unwrap();
    };
    test_retry(r#async);
}

#[test]
fn test_retry_sync() {
    let sync = |client: &RpcClient| {
        client.get_store(1).unwrap();
    };
    test_retry(sync)
}

fn test_not_retry<F: Fn(&RpcClient)>(func: F) {
    let eps_count = 1;
    // NotRetry mocker returns Ok() with error header first, and next returns Ok() without any error header.
    let not_retry = Arc::new(NotRetry::new());
    let server = MockServer::with_case(eps_count, not_retry);
    let eps = server.bind_addrs();

    let client = new_client(eps, None);

    func(&client);
}

#[test]
fn test_not_retry_async() {
    let r#async = |client: &RpcClient| {
        let region = client.get_region_by_id(1);
        region.wait().unwrap_err();
    };
    test_not_retry(r#async);
}

#[test]
fn test_not_retry_sync() {
    let sync = |client: &RpcClient| {
        client.get_store(1).unwrap_err();
    };
    test_not_retry(sync);
}

#[test]
fn test_incompatible_version() {
    let incompatible = Arc::new(Incompatible);
    let server = MockServer::with_case(1, incompatible);
    let eps = server.bind_addrs();

    let client = new_client(eps, None);

    let resp = client.ask_batch_split(metapb::Region::default(), 2);
    assert_eq!(
        resp.wait().unwrap_err().to_string(),
        PdError::Incompatible.to_string()
    );
}

fn restart_leader(mgr: SecurityManager) {
    let mgr = Arc::new(mgr);
    // Service has only one GetMembersResponse, so the leader never changes.
    let mut server =
        MockServer::<Service>::with_configuration(&mgr, vec![("127.0.0.1".to_owned(), 0); 3], None);
    let eps = server.bind_addrs();

    let client = new_client(eps.clone(), Some(Arc::clone(&mgr)));
    // Put a region.
    let store_id = client.alloc_id().unwrap();
    let mut store = metapb::Store::default();
    store.set_id(store_id);

    let peer_id = client.alloc_id().unwrap();
    let mut peer = metapb::Peer::default();
    peer.set_id(peer_id);
    peer.set_store_id(store_id);

    let region_id = client.alloc_id().unwrap();
    let mut region = metapb::Region::default();
    region.set_id(region_id);
    region.mut_peers().push(peer);
    client.bootstrap_cluster(store, region.clone()).unwrap();

    let region = client
        .get_region_by_id(region.get_id())
        .wait()
        .unwrap()
        .unwrap();

    // Stop servers and restart them again.
    server.stop();
    server.start(&mgr, eps);

    // RECONNECT_INTERVAL_SEC is 1s.
    thread::sleep(Duration::from_secs(1));

    let region = client.get_region_by_id(region.get_id()).wait().unwrap();
    assert_eq!(region.unwrap().get_id(), region_id);
}

#[test]
fn test_restart_leader_insecure() {
    let mgr = SecurityManager::new(&SecurityConfig::default()).unwrap();
    restart_leader(mgr)
}

#[test]
fn test_restart_leader_secure() {
    let security_cfg = test_util::new_security_cfg(None);
    let mgr = SecurityManager::new(&security_cfg).unwrap();
    restart_leader(mgr)
}

#[test]
fn test_change_leader_async() {
    let eps_count = 3;
    let server = MockServer::with_case(eps_count, Arc::new(LeaderChange::new()));
    let eps = server.bind_addrs();

    let counter = Arc::new(AtomicUsize::new(0));
    let client = new_client(eps, None);
    let counter1 = Arc::clone(&counter);
    client.handle_reconnect(move || {
        counter1.fetch_add(1, Ordering::SeqCst);
    });
    let leader = client.get_leader();

    for _ in 0..5 {
        let region = client.get_region_by_id(1);
        region.wait().ok();

        let new = client.get_leader();
        if new != leader {
            assert!(counter.load(Ordering::SeqCst) >= 1);
            return;
        }
        thread::sleep(LeaderChange::get_leader_interval());
    }

    panic!("failed, leader should changed");
}

#[test]
fn test_region_heartbeat_on_leader_change() {
    let eps_count = 3;
    let server = MockServer::with_case(eps_count, Arc::new(LeaderChange::new()));
    let eps = server.bind_addrs();

    let client = new_client(eps, None);
    let poller = Builder::new()
        .pool_size(1)
        .name_prefix(thd_name!("poller"))
        .create();
    let (tx, rx) = mpsc::channel();
    let f = client.handle_region_heartbeat_response(1, move |resp| {
        tx.send(resp).unwrap();
    });
    poller.spawn(f).forget();
    let region = metapb::Region::default();
    let peer = metapb::Peer::default();
    let stat = RegionStat::default();
    poller
        .spawn(client.region_heartbeat(
            store::RAFT_INIT_LOG_TERM,
            region.clone(),
            peer.clone(),
            stat.clone(),
        ))
        .forget();
    rx.recv_timeout(LeaderChange::get_leader_interval())
        .unwrap();

    let heartbeat_on_leader_change = |count| {
        let mut leader = client.get_leader();
        for _ in 0..count {
            loop {
                let _ = client.get_region_by_id(1).wait();
                let new = client.get_leader();
                if leader != new {
                    leader = new;
                    info!("leader changed!");
                    break;
                }
                thread::sleep(LeaderChange::get_leader_interval());
            }
        }
        poller
            .spawn(client.region_heartbeat(
                store::RAFT_INIT_LOG_TERM,
                region.clone(),
                peer.clone(),
                stat.clone(),
            ))
            .forget();
        rx.recv_timeout(LeaderChange::get_leader_interval())
            .unwrap();
    };

    // Change PD leader once then heartbeat PD.
    heartbeat_on_leader_change(1);

    // Change PD leader twice without update the heartbeat sender, then heartbeat PD.
    heartbeat_on_leader_change(2);
}

#[test]
fn test_periodical_update() {
    let eps_count = 3;
    let server = MockServer::with_case(eps_count, Arc::new(LeaderChange::new()));
    let eps = server.bind_addrs();

    let counter = Arc::new(AtomicUsize::new(0));
    let client = new_client_with_update_interval(eps, None, ReadableDuration::secs(3));
    let counter1 = Arc::clone(&counter);
    client.handle_reconnect(move || {
        counter1.fetch_add(1, Ordering::SeqCst);
    });
    let leader = client.get_leader();

    for _ in 0..5 {
        let new = client.get_leader();
        if new != leader {
            assert!(counter.load(Ordering::SeqCst) >= 1);
            return;
        }
        thread::sleep(LeaderChange::get_leader_interval());
    }

    panic!("failed, leader should changed");
}
