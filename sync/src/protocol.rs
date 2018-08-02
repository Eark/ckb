#![cfg_attr(feature = "cargo-clippy", allow(needless_pass_by_value))]

use super::compact_block::{short_transaction_id, short_transaction_id_keys, CompactBlock};
use bigint::H256;
use block_process::BlockProcess;
use ckb_chain::chain::ChainProvider;
use ckb_protocol;
use ckb_time::now_ms;
use core::block::{Block, IndexedBlock};
use core::header::IndexedHeader;
use core::transaction::Transaction;
use fnv::{FnvHashMap, FnvHashSet};
use futures::future;
use futures::future::lazy;
use futures::sync::mpsc;
use getdata_process::GetDataProcess;
use getheaders_process::GetHeadersProcess;
use headers_process::HeadersProcess;
use network::NetworkContextExt;
use network::{NetworkContext, NetworkProtocolHandler, PeerId, Severity, TimerToken};
use pool::txs_pool::TransactionPool;
use protobuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use synchronizer::Synchronizer;
use tokio;
// use tokio::prelude::*;
use tokio::prelude::Stream;
use util::Mutex;

use {
    CHAIN_SYNC_TIMEOUT, EVICTION_TEST_RESPONSE_TIME, MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT,
};

pub const SEND_GET_HEADERS_TOKEN: TimerToken = 1;
pub const BLOCK_FETCH_TOKEN: TimerToken = 2;

pub enum Task {
    OnConnected(Box<NetworkContext>, PeerId),
    SendGetHeadersToAll(Box<NetworkContext>),
    FetchBlock(Box<NetworkContext>),
    HandleGetheaders(Box<NetworkContext>, PeerId, ckb_protocol::GetHeaders),
    HandleHeaders(Box<NetworkContext>, PeerId, ckb_protocol::Headers),
    HandleGetdata(Box<NetworkContext>, PeerId, ckb_protocol::GetData),
    // HandleCompactBlock(Box<NetworkContext>, PeerId, ckb_protocol::CompactBlock),
    HandleBlock(Box<NetworkContext>, PeerId, ckb_protocol::Block),
}

fn is_outbound(nc: &NetworkContext, peer: PeerId) -> Option<bool> {
    nc.session_info(peer)
        .map(|session_info| session_info.originated)
}

pub struct SyncProtocol<C> {
    pub synchronizer: Synchronizer<C>,
    pub receiver: Mutex<Option<mpsc::Receiver<Task>>>,
    pub sender: mpsc::Sender<Task>,
}

impl<C: ChainProvider + 'static> SyncProtocol<C> {
    pub fn new(synchronizer: Synchronizer<C>) -> Self {
        let (sender, receiver) = mpsc::channel(65535);
        SyncProtocol {
            synchronizer,
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }

    pub fn start(&self) {
        let receiver = self.receiver.lock().take().expect("start once");
        let synchronizer = self.synchronizer.clone();
        let handler = receiver.for_each(move |task| {
            let synchronizer = synchronizer.clone();
            match task {
                Task::SendGetHeadersToAll(nc) => tokio::spawn(lazy(move || {
                    Self::send_getheaders_to_all(synchronizer, nc);
                    future::ok(())
                })),
                Task::OnConnected(nc, peer) => tokio::spawn(lazy(move || {
                    Self::on_connected(synchronizer, nc.as_ref(), peer);
                    future::ok(())
                })),
                Task::HandleGetheaders(nc, peer, message) => tokio::spawn(lazy(move || {
                    Self::handle_getheaders(synchronizer, nc, peer, &message);
                    future::ok(())
                })),
                Task::HandleHeaders(nc, peer, message) => tokio::spawn(lazy(move || {
                    Self::handle_headers(synchronizer, nc, peer, &message);
                    future::ok(())
                })),
                Task::HandleGetdata(nc, peer, message) => tokio::spawn(lazy(move || {
                    Self::handle_getdata(synchronizer, nc, peer, &message);
                    future::ok(())
                })),
                Task::HandleBlock(nc, peer, message) => tokio::spawn(lazy(move || {
                    Self::handle_block(synchronizer, nc, peer, &message);
                    future::ok(())
                })),
                Task::FetchBlock(nc) => tokio::spawn(lazy(move || {
                    Self::find_blocks_to_fetch(synchronizer, nc);
                    future::ok(())
                })),
                // Task::HandleCompactBlock(nc, peer, message) => tokio::spawn(lazy(move || {
                //     Self::handle_cmpt_block(synchronizer, nc, peer, &message);
                //     future::ok(())
                // })),
            }
        });
        tokio::run(handler);
    }

    pub fn handle_getheaders(
        synchronizer: Synchronizer<C>,
        nc: Box<NetworkContext>,
        peer: PeerId,
        message: &ckb_protocol::GetHeaders,
    ) {
        GetHeadersProcess::new(message, &synchronizer, &peer, nc.as_ref()).execute()
    }

    pub fn handle_headers(
        synchronizer: Synchronizer<C>,
        nc: Box<NetworkContext>,
        peer: PeerId,
        message: &ckb_protocol::Headers,
    ) {
        HeadersProcess::new(message, &synchronizer, &peer, nc.as_ref()).execute()
    }

    fn handle_getdata(
        synchronizer: Synchronizer<C>,
        nc: Box<NetworkContext>,
        peer: PeerId,
        message: &ckb_protocol::GetData,
    ) {
        GetDataProcess::new(message, &synchronizer, &peer, nc.as_ref()).execute()
    }

    // fn handle_cmpt_block(
    //     synchronizer: Synchronizer<C>,
    //     nc: Box<NetworkContext>,
    //     peer: PeerId,
    //     message: &ckb_protocol::CompactBlock,
    // ) {
    //     CompactBlockProcess::new(message, &synchronizer, &peer, nc.as_ref()).execute()
    // }

    fn handle_block(
        synchronizer: Synchronizer<C>,
        nc: Box<NetworkContext>,
        peer: PeerId,
        message: &ckb_protocol::Block,
    ) {
        BlockProcess::new(message, &synchronizer, &peer, nc.as_ref()).execute()
    }

    pub fn find_blocks_to_fetch(synchronizer: Synchronizer<C>, nc: Box<NetworkContext>) {
        let peers: Vec<PeerId> = { synchronizer.peers.state.read().keys().cloned().collect() };
        debug!(target: "sync", "poll find_blocks_to_fetch select peers");
        for peer in peers {
            let ret = synchronizer.get_blocks_to_fetch(peer);
            if let Some(v_fetch) = ret {
                Self::send_block_getdata(&v_fetch, nc.as_ref(), peer);
            }
        }
    }

    fn send_block_getdata(v_fetch: &[H256], nc: &NetworkContext, peer: PeerId) {
        let mut payload = ckb_protocol::Payload::new();
        let mut getdata = ckb_protocol::GetData::new();
        let inventory = v_fetch
            .iter()
            .map(|h| {
                let mut inventory = ckb_protocol::Inventory::new();
                inventory.set_inv_type(ckb_protocol::InventoryType::MSG_BLOCK);
                inventory.set_hash(h.to_vec());
                inventory
            })
            .collect();
        getdata.set_inventory(inventory);
        payload.set_getdata(getdata);

        let _ = nc.send_payload(peer, payload);
        debug!(target: "sync", "send_block_getdata len={:?} to peer={:?}", v_fetch.len() , peer);
    }

    fn on_connected(synchronizer: Synchronizer<C>, nc: &NetworkContext, peer: PeerId) {
        let tip = synchronizer.tip_header();
        let timeout = synchronizer.get_headers_sync_timeout(&tip);

        let protect_outbound = is_outbound(nc, peer).expect("session exist")
            && synchronizer
                .outbound_peers_with_protect
                .load(Ordering::Acquire)
                < MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;

        if protect_outbound {
            synchronizer
                .outbound_peers_with_protect
                .fetch_add(1, Ordering::Release);
        }

        synchronizer
            .peers
            .on_connected(&peer, timeout, protect_outbound);
        synchronizer.n_sync.fetch_add(1, Ordering::Release);
        Self::send_getheaders_to_peer(synchronizer, nc, peer, &tip);
    }

    pub fn eviction(synchronizer: Synchronizer<C>, nc: &NetworkContext) {
        let mut peer_state = synchronizer.peers.state.write();
        let best_known_headers = synchronizer.peers.best_known_headers.read();
        let is_initial_block_download = synchronizer.is_initial_block_download();
        let mut eviction = Vec::new();

        for (peer, state) in peer_state.iter_mut() {
            let now = now_ms();
            // headers_sync_timeout
            if let Some(timeout) = state.headers_sync_timeout {
                if now > timeout && is_initial_block_download && !state.disconnect {
                    eviction.push(*peer);
                    state.disconnect = true;
                    continue;
                }
            }

            if let Some(is_outbound) = is_outbound(nc, *peer) {
                if !state.chain_sync.protect && is_outbound {
                    let best_known_header = best_known_headers.get(peer);
                    let chain_tip = { synchronizer.chain.tip_header().read().clone() };

                    if best_known_header.is_some()
                        && best_known_header.unwrap().total_difficulty >= chain_tip.total_difficulty
                    {
                        if state.chain_sync.timeout != 0 {
                            state.chain_sync.timeout = 0;
                            state.chain_sync.work_header = None;
                            state.chain_sync.sent_getheaders = false;
                        }
                    } else if state.chain_sync.timeout == 0
                        || (best_known_header.is_some() && state.chain_sync.work_header.is_some()
                            && best_known_header.unwrap().total_difficulty
                                >= state
                                    .chain_sync
                                    .work_header
                                    .clone()
                                    .unwrap()
                                    .total_difficulty)
                    {
                        state.chain_sync.timeout = now + CHAIN_SYNC_TIMEOUT;
                        state.chain_sync.work_header = Some(chain_tip);
                        state.chain_sync.sent_getheaders = false;
                    } else if state.chain_sync.timeout > 0 && now > state.chain_sync.timeout {
                        if state.chain_sync.sent_getheaders {
                            eviction.push(*peer);
                            state.disconnect = true;
                        } else {
                            state.chain_sync.sent_getheaders = true;
                            state.chain_sync.timeout = now + EVICTION_TEST_RESPONSE_TIME;
                            Self::send_getheaders_to_peer(
                                synchronizer.clone(),
                                nc,
                                *peer,
                                &state.chain_sync.work_header.clone().unwrap().header,
                            );
                        }
                    }
                }
            }
        }

        for peer in eviction {
            nc.report_peer(peer, Severity::Timeout);
        }
    }

    fn send_getheaders_to_all(synchronizer: Synchronizer<C>, nc: Box<NetworkContext>) {
        let peers: Vec<PeerId> = { synchronizer.peers.state.read().keys().cloned().collect() };
        debug!(target: "sync", "send_getheaders to peers= {:?}", &peers);
        let tip = synchronizer.tip_header();
        for peer in peers {
            Self::send_getheaders_to_peer(synchronizer.clone(), nc.as_ref(), peer, &tip);
        }
    }

    fn send_getheaders_to_peer(
        synchronizer: Synchronizer<C>,
        nc: &NetworkContext,
        peer: PeerId,
        tip: &IndexedHeader,
    ) {
        let locator_hash = synchronizer.get_locator(tip);
        let mut payload = ckb_protocol::Payload::new();
        let mut getheaders = ckb_protocol::GetHeaders::new();
        let locator_hash = locator_hash.into_iter().map(|hash| hash.to_vec()).collect();
        getheaders.set_version(0);
        getheaders.set_block_locator_hashes(locator_hash);
        getheaders.set_hash_stop(H256::zero().to_vec());
        payload.set_getheaders(getheaders);
        let _ = nc.send_payload(peer, payload);
        debug!(target: "sync", "send_getheaders_to_peer getheaders {:?} to peer={:?}", tip.number ,peer);
    }

    fn dispatch_getheaders(&self, nc: Box<NetworkContext>) {
        if self.synchronizer.n_sync.load(Ordering::Acquire) == 0
            || !self.synchronizer.is_initial_block_download()
        {
            debug!(target: "sync", "dispatch_getheaders");
            let mut sender = self.sender.clone();
            let ret = sender.try_send(Task::SendGetHeadersToAll(nc));

            if ret.is_err() {
                error!(target: "sync", "dispatch_getheaders peer error {:?}", ret);
            }
        }
    }

    fn dispatch_block_fetch(&self, nc: Box<NetworkContext>) {
        debug!(target: "sync", "dispatch_block_download");
        let mut sender = self.sender.clone();
        let ret = sender.try_send(Task::FetchBlock(nc));

        if ret.is_err() {
            error!(target: "sync", "dispatch_block_download peer error {:?}", ret);
        }
    }

    fn dispatch_on_connected(&self, nc: Box<NetworkContext>, peer: PeerId) {
        if self.synchronizer.n_sync.load(Ordering::Acquire) == 0
            || !self.synchronizer.is_initial_block_download()
        {
            debug!(target: "sync", "init_getheaders peer={:?} connected", peer);
            let mut sender = self.sender.clone();
            let ret = sender.try_send(Task::OnConnected(nc, peer));

            if ret.is_err() {
                error!(target: "sync", "init_getheaders peer={:?} error {:?}", peer, ret);
            }
        }
    }

    fn process(&self, nc: Box<NetworkContext>, peer: &PeerId, mut payload: ckb_protocol::Payload) {
        let mut sender = self.sender.clone();
        let ret = if payload.has_getheaders() {
            sender.try_send(Task::HandleGetheaders(nc, *peer, payload.take_getheaders()))
        } else if payload.has_headers() {
            let headers = payload.take_headers();
            debug!(target: "sync", "receive headers massge {}", headers.headers.len());
            sender.try_send(Task::HandleHeaders(nc, *peer, headers))
        } else if payload.has_getdata() {
            sender.try_send(Task::HandleGetdata(nc, *peer, payload.take_getdata()))
        } else if payload.has_block() {
            sender.try_send(Task::HandleBlock(nc, *peer, payload.take_block()))
        } else {
            Ok(())
        };

        if ret.is_err() {
            error!(target: "sync", "NetworkProtocolHandler dispatch message error {:?}", ret);
        }
    }
}

impl<C: ChainProvider + 'static> NetworkProtocolHandler for SyncProtocol<C> {
    fn initialize(&self, nc: Box<NetworkContext>) {
        // NOTE: 100ms is what bitcoin use.
        let _ = nc.register_timer(SEND_GET_HEADERS_TOKEN, Duration::from_millis(100));
        let _ = nc.register_timer(BLOCK_FETCH_TOKEN, Duration::from_millis(100));
    }

    /// Called when new network packet received.
    fn read(&self, nc: Box<NetworkContext>, peer: &PeerId, _packet_id: u8, data: &[u8]) {
        match protobuf::parse_from_bytes::<ckb_protocol::Payload>(data) {
            Ok(payload) => self.process(nc, peer, payload),
            Err(err) => warn!(target: "sync", "Failed to parse protobuf, error={:?}", err),
        };
    }

    fn connected(&self, nc: Box<NetworkContext>, peer: &PeerId) {
        info!(target: "sync", "peer={} SyncProtocol.connected", peer);
        self.dispatch_on_connected(nc, *peer);
    }

    fn disconnected(&self, _nc: Box<NetworkContext>, peer: &PeerId) {
        info!(target: "sync", "peer={} SyncProtocol.disconnected", peer);
        self.synchronizer.peers.disconnected(&peer);
    }

    fn timeout(&self, nc: Box<NetworkContext>, token: TimerToken) {
        if !self.synchronizer.peers.state.read().is_empty() {
            match token as usize {
                SEND_GET_HEADERS_TOKEN => self.dispatch_getheaders(nc),
                BLOCK_FETCH_TOKEN => self.dispatch_block_fetch(nc),
                _ => unreachable!(),
            }
        } else {
            debug!(target: "sync", "no peers connected");
        }
    }
}

pub struct RelayProtocol<C> {
    pub synchronizer: Synchronizer<C>,
    pub tx_pool: Arc<TransactionPool<C>>,
    // TODO add size limit or use bloom filter
    pub received_blocks: Mutex<FnvHashSet<H256>>,
    pub received_transactions: Mutex<FnvHashSet<H256>>,
    pub pending_compact_blocks: Mutex<FnvHashMap<H256, CompactBlock>>,
}

impl<C: ChainProvider + 'static> RelayProtocol<C> {
    pub fn new(synchronizer: Synchronizer<C>, tx_pool: &Arc<TransactionPool<C>>) -> Self {
        RelayProtocol {
            synchronizer,
            tx_pool: Arc::clone(tx_pool),
            received_blocks: Mutex::new(FnvHashSet::default()),
            received_transactions: Mutex::new(FnvHashSet::default()),
            pending_compact_blocks: Mutex::new(FnvHashMap::default()),
        }
    }

    pub fn relay(&self, nc: &NetworkContext, source: PeerId, payload: &ckb_protocol::Payload) {
        let peer_ids = self
            .synchronizer
            .peers
            .state
            .read()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for (peer_id, _session) in nc.sessions(&peer_ids) {
            if peer_id != source {
                let _ = nc.send_payload(peer_id, payload.clone());
            }
        }
    }

    fn reconstruct_block(
        &self,
        compact_block: &CompactBlock,
        transactions: Vec<Transaction>,
    ) -> (Option<IndexedBlock>, Option<Vec<usize>>) {
        let (key0, key1) = short_transaction_id_keys(compact_block.nonce, &compact_block.header);

        let mut txs = transactions;
        txs.extend(self.tx_pool.pool.read().pool.get_vertices());
        txs.extend(self.tx_pool.orphan.read().pool.get_vertices());

        let mut txs_map = FnvHashMap::default();
        for tx in txs {
            let short_id = short_transaction_id(key0, key1, &tx.hash());
            txs_map.insert(short_id, tx);
        }

        let mut block_transactions = Vec::with_capacity(compact_block.short_ids.len());
        let mut missing_indexes = Vec::new();
        for (index, short_id) in compact_block.short_ids.iter().enumerate() {
            match txs_map.remove(short_id) {
                Some(tx) => block_transactions.insert(index, tx),
                None => missing_indexes.push(index),
            }
        }

        if missing_indexes.is_empty() {
            let block = Block::new(
                compact_block.header.clone(),
                block_transactions,
                compact_block.uncles.clone(),
            );

            (Some(block.into()), None)
        } else {
            (None, Some(missing_indexes))
        }
    }

    fn process(&self, nc: Box<NetworkContext>, peer: &PeerId, payload: ckb_protocol::Payload) {
        if payload.has_transaction() {
            let tx: Transaction = payload.get_transaction().into();
            if !self.received_transactions.lock().insert(tx.hash()) {
                let _ = self.tx_pool.add_to_memory_pool(tx);
                self.relay(nc.as_ref(), *peer, &payload);
            }
        } else if payload.has_block() {
            let block: Block = payload.get_block().into();
            if !self.received_blocks.lock().insert(block.hash()) {
                self.synchronizer.process_new_block(*peer, block.into());
                self.relay(nc.as_ref(), *peer, &payload);
            }
        } else if payload.has_compact_block() {
            let compact_block: CompactBlock = payload.get_compact_block().into();
            debug!(target: "sync", "receive compact block from peer#{}, {} => {}",
                   peer,
                   compact_block.header().number,
                   compact_block.header().hash(),
            );
            if !self
                .received_blocks
                .lock()
                .insert(compact_block.header.hash())
            {
                match self.reconstruct_block(&compact_block, Vec::new()) {
                    (Some(block), _) => {
                        self.synchronizer.process_new_block(*peer, block);
                        self.relay(nc.as_ref(), *peer, &payload);
                    }
                    (_, Some(missing_indexes)) => {
                        let mut payload = ckb_protocol::Payload::new();
                        let mut cbr = ckb_protocol::BlockTransactionsRequest::new();
                        cbr.set_hash(compact_block.header.hash().to_vec());
                        cbr.set_indexes(missing_indexes.into_iter().map(|i| i as u32).collect());
                        payload.set_block_transactions_request(cbr);
                        self.pending_compact_blocks
                            .lock()
                            .insert(compact_block.header.hash(), compact_block);
                        let _ = nc.respond_payload(payload);
                    }
                    (None, None) => {
                        // TODO fail to reconstruct block, downgrade to header first?
                    }
                }
            }
        } else if payload.has_block_transactions_request() {
            let btr = payload.get_block_transactions_request();
            let hash = H256::from_slice(btr.get_hash());
            let indexes = btr.get_indexes();
            if let Some(block) = self.synchronizer.get_block(&hash) {
                let mut payload = ckb_protocol::Payload::new();
                let mut bt = ckb_protocol::BlockTransactions::new();
                bt.set_hash(hash.to_vec());
                bt.set_transactions(
                    indexes
                        .iter()
                        .filter_map(|i| block.transactions.get(*i as usize))
                        .map(Into::into)
                        .collect(),
                );
                let _ = nc.respond_payload(payload);
            }
        } else if payload.has_block_transactions() {
            let bt = payload.get_block_transactions();
            let hash = H256::from_slice(bt.get_hash());
            if let Some(compact_block) = self.pending_compact_blocks.lock().remove(&hash) {
                let transactions: Vec<Transaction> =
                    bt.get_transactions().iter().map(Into::into).collect();
                if let (Some(block), _) = self.reconstruct_block(&compact_block, transactions) {
                    self.synchronizer.process_new_block(*peer, block);
                }
            }
        }
    }
}

impl<C: ChainProvider + 'static> NetworkProtocolHandler for RelayProtocol<C> {
    /// Called when new network packet received.
    fn read(&self, nc: Box<NetworkContext>, peer: &PeerId, _packet_id: u8, data: &[u8]) {
        match protobuf::parse_from_bytes::<ckb_protocol::Payload>(data) {
            Ok(payload) => self.process(nc, peer, payload),
            Err(err) => warn!(target: "sync", "Failed to parse protobuf, error={:?}", err),
        };
    }

    fn connected(&self, _nc: Box<NetworkContext>, peer: &PeerId) {
        info!(target: "sync", "peer={} RelayProtocol.connected", peer);
        // do nothing
    }

    fn disconnected(&self, _nc: Box<NetworkContext>, peer: &PeerId) {
        info!(target: "sync", "peer={} RelayProtocol.disconnected", peer);
        // TODO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigint::U256;
    use ckb_chain::chain::Chain;
    use ckb_chain::consensus::Consensus;
    use ckb_chain::store::ChainKVStore;
    use ckb_chain::COLUMNS;
    use ckb_notify::Notify;
    use ckb_time::{now_ms, set_mock_timer};
    use config::Config;
    use db::memorydb::MemoryKeyValueDB;
    use header_view::HeaderView;
    use network::{
        Error as NetworkError, NetworkContext, PacketId, PeerId, ProtocolId, SessionInfo, Severity,
        TimerToken,
    };
    use std::iter::FromIterator;
    use std::ops::Deref;
    use std::time::Duration;
    use MAX_TIP_AGE;

    fn mock_session_info() -> SessionInfo {
        SessionInfo {
            id: None,
            client_version: "mock".to_string(),
            protocol_version: 0,
            capabilities: vec![],
            peer_capabilities: vec![],
            ping: None,
            originated: true,
            remote_address: "mock".to_string(),
            local_address: "mock".to_string(),
        }
    }

    fn mock_header_view(total_difficulty: u64) -> HeaderView {
        HeaderView {
            total_difficulty: U256::from(total_difficulty),
            header: IndexedHeader::default(),
        }
    }

    fn gen_chain(consensus: &Consensus) -> Chain<ChainKVStore<MemoryKeyValueDB>> {
        let db = MemoryKeyValueDB::open(COLUMNS as usize);
        let store = ChainKVStore { db };
        let chain = Chain::init(store, consensus.clone(), Notify::default()).unwrap();
        chain
    }

    #[derive(Clone)]
    struct DummyNetworkContext {
        pub sessions: FnvHashMap<PeerId, SessionInfo>,
        pub disconnected: Arc<Mutex<FnvHashSet<PeerId>>>,
    }

    impl NetworkContext for DummyNetworkContext {
        /// Send a packet over the network to another peer.
        fn send(&self, _peer: PeerId, _packet_id: PacketId, _data: Vec<u8>) {}

        /// Send a packet over the network to another peer using specified protocol.
        fn send_protocol(
            &self,
            _protocol: ProtocolId,
            _peer: PeerId,
            _packet_id: PacketId,
            _data: Vec<u8>,
        ) {
        }

        /// Respond to a current network message. Panics if no there is no packet in the context. If the session is expired returns nothing.
        fn respond(&self, _packet_id: PacketId, _data: Vec<u8>) {
            unimplemented!();
        }

        /// Report peer. Depending on the report, peer may be disconnected and possibly banned.
        fn report_peer(&self, peer: PeerId, _reason: Severity) {
            self.disconnected.lock().insert(peer);
        }

        /// Check if the session is still active.
        fn is_expired(&self) -> bool {
            false
        }

        /// Register a new IO timer. 'IoHandler::timeout' will be called with the token.
        fn register_timer(&self, _token: TimerToken, _delay: Duration) -> Result<(), NetworkError> {
            unimplemented!();
        }

        /// Returns peer identification string
        fn peer_client_version(&self, _peer: PeerId) -> String {
            unimplemented!();
        }

        /// Returns information on p2p session
        fn session_info(&self, peer: PeerId) -> Option<SessionInfo> {
            self.sessions.get(&peer).cloned()
        }

        /// Returns max version for a given protocol.
        fn protocol_version(&self, _protocol: ProtocolId, _peer: PeerId) -> Option<u8> {
            unimplemented!();
        }

        /// Returns this object's subprotocol name.
        fn subprotocol_name(&self) -> ProtocolId {
            unimplemented!();
        }
    }

    fn mock_network_context(peer_num: usize) -> DummyNetworkContext {
        let mut sessions = FnvHashMap::default();
        for peer in 0..peer_num {
            sessions.insert(peer, mock_session_info());
        }
        DummyNetworkContext {
            sessions,
            disconnected: Arc::new(Mutex::new(FnvHashSet::default())),
        }
    }

    #[test]
    fn test_header_sync_timeout() {
        let config = Consensus::default();
        let chain = Arc::new(gen_chain(&config));

        let synchronizer = Synchronizer::new(&chain, None, Config::default());

        let network_context = mock_network_context(5);

        set_mock_timer(MAX_TIP_AGE * 2);

        assert!(synchronizer.is_initial_block_download());

        let peers = synchronizer.peers();
        // protect should not effect headers_timeout
        peers.on_connected(&0, 0, true);
        peers.on_connected(&1, 0, false);
        peers.on_connected(&2, MAX_TIP_AGE * 2, false);

        SyncProtocol::eviction(synchronizer, &network_context);

        let disconnected = network_context.disconnected.lock();

        assert_eq!(
            disconnected.deref(),
            &FnvHashSet::from_iter(vec![0, 1].into_iter())
        )
    }

    #[test]
    fn test_chain_sync_timeout() {
        let mut consensus = Consensus::default();
        consensus.genesis_block.header.raw.difficulty = U256::from(2);
        let chain = Arc::new(gen_chain(&consensus));

        assert_eq!(chain.tip_header().read().total_difficulty, U256::from(2));

        let synchronizer = Synchronizer::new(&chain, None, Config::default());

        let network_context = mock_network_context(6);

        let peers = synchronizer.peers();

        //6 peers do not trigger header sync timeout
        peers.on_connected(&0, MAX_TIP_AGE * 2, true);
        peers.on_connected(&1, MAX_TIP_AGE * 2, true);
        peers.on_connected(&2, MAX_TIP_AGE * 2, true);
        peers.on_connected(&3, MAX_TIP_AGE * 2, false);
        peers.on_connected(&4, MAX_TIP_AGE * 2, false);
        peers.on_connected(&5, MAX_TIP_AGE * 2, false);

        peers.new_header_received(&0, &mock_header_view(1));
        peers.new_header_received(&2, &mock_header_view(3));
        peers.new_header_received(&3, &mock_header_view(1));
        peers.new_header_received(&5, &mock_header_view(3));

        SyncProtocol::eviction(synchronizer.clone(), &network_context);

        {
            assert!({ network_context.disconnected.lock().is_empty() });
            let peer_state = peers.state.read();

            assert_eq!(peer_state.get(&0).unwrap().chain_sync.protect, true);
            assert_eq!(peer_state.get(&1).unwrap().chain_sync.protect, true);
            assert_eq!(peer_state.get(&2).unwrap().chain_sync.protect, true);
            //protect peer is protected from disconnection
            assert!(peer_state.get(&2).unwrap().chain_sync.work_header.is_none());

            assert_eq!(peer_state.get(&3).unwrap().chain_sync.protect, false);
            assert_eq!(peer_state.get(&4).unwrap().chain_sync.protect, false);
            assert_eq!(peer_state.get(&5).unwrap().chain_sync.protect, false);

            // Our best block known by this peer is behind our tip, and we're either noticing
            // that for the first time, OR this peer was able to catch up to some earlier point
            // where we checked against our tip.
            // Either way, set a new timeout based on current tip.
            let tip = { chain.tip_header().read().clone() };
            assert_eq!(
                peer_state.get(&3).unwrap().chain_sync.work_header,
                Some(tip.clone())
            );
            assert_eq!(
                peer_state.get(&4).unwrap().chain_sync.work_header,
                Some(tip)
            );
            assert_eq!(
                peer_state.get(&3).unwrap().chain_sync.timeout,
                CHAIN_SYNC_TIMEOUT
            );
            assert_eq!(
                peer_state.get(&4).unwrap().chain_sync.timeout,
                CHAIN_SYNC_TIMEOUT
            );
        }

        set_mock_timer(CHAIN_SYNC_TIMEOUT + 1);
        SyncProtocol::eviction(synchronizer.clone(), &network_context);
        {
            let peer_state = peers.state.read();
            // No evidence yet that our peer has synced to a chain with work equal to that
            // of our tip, when we first detected it was behind. Send a single getheaders
            // message to give the peer a chance to update us.
            assert!({ network_context.disconnected.lock().is_empty() });

            assert_eq!(
                peer_state.get(&3).unwrap().chain_sync.timeout,
                now_ms() + EVICTION_TEST_RESPONSE_TIME
            );
            assert_eq!(
                peer_state.get(&4).unwrap().chain_sync.timeout,
                now_ms() + EVICTION_TEST_RESPONSE_TIME
            );
        }

        set_mock_timer(now_ms() + EVICTION_TEST_RESPONSE_TIME + 1);
        SyncProtocol::eviction(synchronizer, &network_context);

        {
            // Peer(3,4) run out of time to catch up!
            let disconnected = network_context.disconnected.lock();
            assert_eq!(
                disconnected.deref(),
                &FnvHashSet::from_iter(vec![3, 4].into_iter())
            )
        }
    }
}
