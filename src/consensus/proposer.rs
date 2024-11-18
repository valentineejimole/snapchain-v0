use crate::consensus::consensus::{RxDecision, TxDecision};
use crate::core::types::{
    proto, Address, Height, ShardHash, ShardId, SnapchainShard, SnapchainValidator,
};
use crate::proto::rpc::snapchain_service_client::SnapchainServiceClient;
use crate::proto::rpc::{BlocksRequest, ShardChunksRequest};
use crate::proto::snapchain::{Block, BlockHeader, FullProposal, ShardChunk, ShardHeader};
use crate::storage::store::engine::{BlockEngine, ShardEngine, ShardStateChange};
use crate::storage::store::BlockStorageError;
use malachite_common::{Round, Validity};
use prost::Message;
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::{select, time};
use tonic::Request;
use tracing::{error, warn};

const FARCASTER_EPOCH: u64 = 1609459200; // January 1, 2021 UTC

pub fn current_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - FARCASTER_EPOCH
}

pub trait Proposer {
    // Create a new block/shard chunk for the given height that will be proposed for confirmation to the other validators
    async fn propose_value(
        &mut self,
        height: Height,
        round: Round,
        timeout: Duration,
    ) -> FullProposal;
    // Receive a block/shard chunk proposed by another validator and return whether it is valid
    fn add_proposed_value(&mut self, full_proposal: &FullProposal) -> Validity;

    // Consensus has confirmed the block/shard_chunk, apply it to the local state
    async fn decide(&mut self, height: Height, round: Round, value: ShardHash);

    fn get_confirmed_height(&self) -> Height;

    async fn register_validator(
        &mut self,
        validator: &SnapchainValidator,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

pub struct ShardProposer {
    shard_id: SnapchainShard,
    address: Address,
    chunks: Vec<ShardChunk>,
    proposed_chunks: BTreeMap<ShardHash, FullProposal>,
    tx_decision: Option<mpsc::Sender<ShardChunk>>,
    engine: ShardEngine,
}

impl ShardProposer {
    pub fn new(
        address: Address,
        shard_id: SnapchainShard,
        engine: ShardEngine,
        tx_decision: Option<mpsc::Sender<ShardChunk>>,
    ) -> ShardProposer {
        ShardProposer {
            shard_id,
            address,
            chunks: vec![],
            proposed_chunks: BTreeMap::new(),
            tx_decision,
            engine,
        }
    }

    async fn publish_new_shard_chunk(&self, shard_chunk: ShardChunk) {
        if let Some(tx_decision) = &self.tx_decision {
            let _ = tx_decision.send(shard_chunk).await;
        }
    }
}

impl Proposer for ShardProposer {
    async fn propose_value(
        &mut self,
        height: Height,
        round: Round,
        _timeout: Duration,
    ) -> FullProposal {
        // Sleep before proposing the value so we don't produce blocks too fast
        // TODO: rethink/reconsider
        // tokio::time::sleep(Duration::from_millis(250)).await;

        let previous_chunk = self.chunks.last();
        let parent_hash = match previous_chunk {
            Some(chunk) => chunk.hash.clone(),
            None => vec![0, 32],
        };

        let state_change = self.engine.propose_state_change(self.shard_id.shard_id());
        let shard_header = ShardHeader {
            parent_hash,
            timestamp: current_time(),
            height: Some(height.clone()),
            shard_root: state_change.new_state_root.clone(),
        };
        let hash = blake3::hash(&shard_header.encode_to_vec())
            .as_bytes()
            .to_vec();

        let chunk = ShardChunk {
            header: Some(shard_header),
            hash: hash.clone(),
            transactions: state_change.transactions.clone(),
            votes: None,
        };

        let shard_hash = ShardHash {
            hash: hash.clone(),
            shard_index: height.shard_index as u32,
        };
        let proposal = FullProposal {
            height: Some(height.clone()),
            round: round.as_i64(),
            proposed_value: Some(proto::full_proposal::ProposedValue::Shard(chunk)),
            proposer: self.address.to_vec(),
        };
        self.proposed_chunks.insert(shard_hash, proposal.clone());
        proposal
    }

    fn add_proposed_value(&mut self, full_proposal: &FullProposal) -> Validity {
        if let Some(proto::full_proposal::ProposedValue::Shard(chunk)) =
            full_proposal.proposed_value.clone()
        {
            self.proposed_chunks
                .insert(full_proposal.shard_hash(), full_proposal.clone());
            let state = ShardStateChange {
                shard_id: chunk.header.clone().unwrap().height.unwrap().shard_index,
                new_state_root: chunk.header.clone().unwrap().shard_root.clone(),
                transactions: chunk.transactions.clone(),
            };
            return if self.engine.validate_state_change(&state) {
                Validity::Valid
            } else {
                error!("Invalid state change for shard: {:?}", state.shard_id);
                Validity::Invalid
            };
        }
        error!("Invalid proposed value: {:?}", full_proposal.proposed_value);
        Validity::Invalid // TODO: Validate proposer signature?
    }

    async fn decide(&mut self, _height: Height, _round: Round, value: ShardHash) {
        if let Some(proposal) = self.proposed_chunks.get(&value) {
            self.publish_new_shard_chunk(proposal.shard_chunk().unwrap())
                .await;
            self.chunks.push(proposal.shard_chunk().unwrap());
            self.engine
                .commit_shard_chunk(proposal.shard_chunk().unwrap());
            self.proposed_chunks.remove(&value);
        }
    }

    fn get_confirmed_height(&self) -> Height {
        self.engine.get_confirmed_height()
    }

    async fn register_validator(
        &mut self,
        validator: &SnapchainValidator,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let prev_block_number = self.engine.get_confirmed_height().block_number;

        if validator.current_height > prev_block_number {
            match &validator.rpc_address {
                None => return Ok(()),
                Some(rpc_address) => {
                    let destination_addr = format!("http://{}", rpc_address.clone());
                    let mut rpc_client = SnapchainServiceClient::connect(destination_addr).await?;
                    let request = Request::new(ShardChunksRequest {
                        shard_id: self.shard_id.shard_id(),
                        start_block_number: prev_block_number + 1,
                        stop_block_number: None,
                    });
                    let missing_shard_chunks = rpc_client.get_shard_chunks(request).await?;
                    for shard_chunk in missing_shard_chunks.get_ref().shard_chunks.clone() {
                        self.publish_new_shard_chunk(shard_chunk).await;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Error, Debug)]
pub enum BlockProposerError {
    #[error("Block missing header")]
    BlockMissingHeader,

    #[error("Block missing height")]
    BlockMissingHeight,

    #[error("No peers")]
    NoPeers,

    #[error(transparent)]
    RpcTransportError(#[from] tonic::transport::Error),

    #[error(transparent)]
    RpcResponseError(#[from] tonic::Status),

    #[error(transparent)]
    BlockStorageError(#[from] BlockStorageError),
}

pub struct BlockProposer {
    shard_id: SnapchainShard,
    address: Address,
    proposed_blocks: BTreeMap<ShardHash, FullProposal>,
    pending_chunks: BTreeMap<u64, Vec<ShardChunk>>,
    shard_decision_rx: mpsc::Receiver<ShardChunk>,
    num_shards: u32,
    block_tx: mpsc::Sender<Block>,
    engine: BlockEngine,
}

impl BlockProposer {
    pub fn new(
        address: Address,
        shard_id: SnapchainShard,
        shard_decision_rx: mpsc::Receiver<ShardChunk>,
        num_shards: u32,
        block_tx: mpsc::Sender<Block>,
        engine: BlockEngine,
    ) -> BlockProposer {
        BlockProposer {
            shard_id,
            address,
            proposed_blocks: BTreeMap::new(),
            pending_chunks: BTreeMap::new(),
            shard_decision_rx,
            num_shards,
            block_tx,
            engine,
        }
    }

    async fn collect_confirmed_shard_chunks(
        &mut self,
        height: Height,
        timeout: Duration,
    ) -> Vec<ShardChunk> {
        let requested_height = height.block_number;

        let mut poll_interval = time::interval(Duration::from_millis(10));

        // convert to deadline
        let deadline = Instant::now() + timeout;
        loop {
            let timeout = time::sleep_until(deadline);
            select! {
                _ = poll_interval.tick() => {
                    // TODO(aditi): This breaks if syncd shard chunks show up in shard_decision_rx.
                    if let Ok(chunk) = self.shard_decision_rx.try_recv() {
                        let chunk_height = chunk.header.clone().unwrap().height.unwrap();
                        let chunk_block_number = chunk_height.block_number;
                        if self.pending_chunks.contains_key(&chunk_block_number) {
                            self.pending_chunks.get_mut(&chunk_block_number).unwrap().push(chunk);
                        } else {
                            self.pending_chunks.insert(chunk_block_number, vec![chunk]);
                        }
                    }
                    if let Some(chunks) = self.pending_chunks.get(&requested_height) {
                        if chunks.len() == self.num_shards as usize {
                            break;
                        }
                    }
                }
                _ = timeout => {
                    warn!("Block validator did not receive all shard chunks in time for height: {:?}", requested_height);
                    break;
                }
            }
        }

        if let Some(chunks) = self.pending_chunks.get(&requested_height) {
            chunks.clone()
        } else {
            vec![]
        }
    }

    async fn publish_new_block(&self, block: Block) {
        match self.block_tx.send(block.clone()).await {
            Err(err) => {
                error!("Erorr publishing new block {:#?}", err)
            }
            Ok(_) => {}
        }
    }
}

impl Proposer for BlockProposer {
    async fn propose_value(
        &mut self,
        height: Height,
        round: Round,
        timeout: Duration,
    ) -> FullProposal {
        let shard_chunks = self.collect_confirmed_shard_chunks(height, timeout).await;

        let previous_block = self.engine.get_last_block();
        let parent_hash = match previous_block {
            Some(block) => block.hash.clone(),
            None => vec![0, 32],
        };
        let block_header = BlockHeader {
            parent_hash,
            chain_id: 0,
            version: 0,
            shard_headers_hash: vec![],
            validators_hash: vec![],
            timestamp: current_time(),
            height: Some(height.clone()),
        };
        let hash = blake3::hash(&block_header.encode_to_vec())
            .as_bytes()
            .to_vec();

        let block = Block {
            header: Some(block_header),
            hash: hash.clone(),
            validators: None,
            votes: None,
            shard_chunks,
        };

        let shard_hash = ShardHash {
            hash: hash.clone(),
            shard_index: height.shard_index as u32,
        };

        let proposal = FullProposal {
            height: Some(height.clone()),
            round: round.as_i64(),
            proposed_value: Some(proto::full_proposal::ProposedValue::Block(block)),
            proposer: self.address.to_vec(),
        };

        self.proposed_blocks.insert(shard_hash, proposal.clone());
        proposal
    }

    fn add_proposed_value(&mut self, full_proposal: &FullProposal) -> Validity {
        if let Some(proto::full_proposal::ProposedValue::Block(_block)) =
            full_proposal.proposed_value.clone()
        {
            self.proposed_blocks
                .insert(full_proposal.shard_hash(), full_proposal.clone());
        }
        Validity::Valid // TODO: Validate proposer signature?
    }

    async fn decide(&mut self, height: Height, _round: Round, value: ShardHash) {
        if let Some(proposal) = self.proposed_blocks.get(&value) {
            self.engine.commit_block(proposal.block().unwrap());

            self.publish_new_block(proposal.block().unwrap()).await;

            self.proposed_blocks.remove(&value);
            self.pending_chunks.remove(&height.block_number);
        }
    }

    fn get_confirmed_height(&self) -> Height {
        self.engine.get_confirmed_height()
    }

    async fn register_validator(
        &mut self,
        validator: &SnapchainValidator,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let prev_block_number = self.engine.get_confirmed_height().block_number;

        if validator.current_height > prev_block_number {
            match &validator.rpc_address {
                None => return Ok(()),
                Some(rpc_address) => {
                    let destination_addr = format!("http://{}", rpc_address.clone());
                    let mut rpc_client = SnapchainServiceClient::connect(destination_addr).await?;
                    let request = Request::new(BlocksRequest {
                        shard_id: self.shard_id.shard_id(),
                        start_block_number: prev_block_number + 1,
                        stop_block_number: None,
                    });
                    let missing_blocks = rpc_client.get_blocks(request).await?;
                    for block in missing_blocks.get_ref().blocks.clone() {
                        self.publish_new_block(block).await;
                    }
                }
            }
        }

        Ok(())
    }
}
