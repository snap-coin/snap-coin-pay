use std::{net::SocketAddr, time::Duration};

use snap_coin::{
    UtilError,
    api::client::Client,
    blockchain_data_provider::{BlockchainDataProvider, BlockchainDataProviderError},
    build_transaction,
    core::{
        blockchain::{Blockchain, BlockchainError},
        difficulty::calculate_live_transaction_difficulty,
        transaction::{Transaction, TransactionInput},
    },
    crypto::keys::{Private, Public},
    full_node::{
        SharedBlockchain, accept_transaction,
        node_state::{ChainEvent, SharedNodeState},
    },
};
use thiserror::Error;
use tokio::sync::{RwLock, broadcast};

#[derive(Error, Debug)]
pub enum ChainInteractionError {
    #[error("Other: {0}")]
    Other(String),
    #[error("{0}")]
    IoError(#[from] std::io::Error),
    #[error("{0}")]
    BlockchainDataProvider(#[from] BlockchainDataProviderError),
    #[error("{0}")]
    UtilError(#[from] UtilError),
    #[error("{0}")]
    BlockchainError(#[from] BlockchainError),
    #[error("Transaction doesn't have a transaction id attached")]
    MissingTransactionId,
    #[error("Failed to submit transaction {0}")]
    SubmissionFailed(String),
}

#[async_trait::async_trait]
pub trait ChainInteraction: Send + Sync + 'static {
    type Provider: BlockchainDataProvider;

    async fn start_listener(
        &self,
        shutdown: Option<broadcast::Receiver<()>>,
    ) -> Result<(), ChainInteractionError>;
    fn subscribe(&self) -> broadcast::Receiver<ChainEvent>;
    fn get_blockchain_data_provider(&self) -> &Self::Provider;
    async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<Result<(), BlockchainError>, ChainInteractionError>;
    async fn build_transaction(
        &self,
        sender: Private,
        receivers: Vec<(Public, u64)>,
    ) -> Result<Transaction, ChainInteractionError>;

    async fn on_expiry(&self, transaction: Transaction);
}

pub struct ApiChainInteraction {
    tx: broadcast::Sender<ChainEvent>,
    api: SocketAddr,
    client: Client,
    spent_inputs: RwLock<Vec<TransactionInput>>,
}

impl ApiChainInteraction {
    pub async fn new(api: SocketAddr) -> Result<Self, ChainInteractionError> {
        Ok(Self {
            tx: broadcast::channel(24).0,
            api,
            client: Client::connect(api).await?,
            spent_inputs: RwLock::new(vec![]),
        })
    }
}

#[async_trait::async_trait]
impl ChainInteraction for ApiChainInteraction {
    type Provider = Client;

    async fn start_listener(
        &self,
        shutdown: Option<broadcast::Receiver<()>>,
    ) -> Result<(), ChainInteractionError> {
        let client = Client::connect(self.api).await?;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            client
                .convert_to_event_listener(
                    move |event| {
                        tx.send(event).ok();
                    },
                    shutdown,
                )
                .await
                .ok();
        });

        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        self.tx.subscribe()
    }

    fn get_blockchain_data_provider(&self) -> &Client {
        &self.client
    }

    async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<Result<(), BlockchainError>, ChainInteractionError> {
        Ok(self.client.submit_transaction(transaction).await?)
    }

    async fn build_transaction(
        &self,
        sender: Private,
        receivers: Vec<(Public, u64)>,
    ) -> Result<Transaction, ChainInteractionError> {
        let mut tx = build_transaction(
            &self.client,
            sender,
            receivers,
            &*self.spent_inputs.read().await,
        )
        .await?;
        tx.compute_pow(
            &self.client.get_live_transaction_difficulty().await?,
            Some(0.1),
        )
        .map_err(|e| BlockchainError::BincodeEncode(e.to_string()))?;
        self.spent_inputs
            .write()
            .await
            .extend_from_slice(&tx.inputs);

        Ok(tx)
    }

    async fn on_expiry(&self, transaction: Transaction) {
        let mut spent_inputs = self.spent_inputs.write().await;
        spent_inputs.retain(|input| !transaction.inputs.contains(input));
    }
}

pub struct NodeChainInteraction {
    tx: broadcast::Sender<ChainEvent>,
    node_state: SharedNodeState,
    blockchain: SharedBlockchain,
    spent_inputs: RwLock<Vec<TransactionInput>>,
}

impl NodeChainInteraction {
    pub fn new(node_state: SharedNodeState, blockchain: SharedBlockchain) -> Self {
        Self {
            tx: broadcast::channel(24).0,
            node_state,
            blockchain,
            spent_inputs: RwLock::new(vec![]),
        }
    }
}

#[async_trait::async_trait]
impl ChainInteraction for NodeChainInteraction {
    type Provider = Blockchain;

    async fn start_listener(
        &self,
        mut shutdown: Option<broadcast::Receiver<()>>,
    ) -> Result<(), ChainInteractionError> {
        let mut rx = self.node_state.chain_events.subscribe();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            'listener: loop {
                let event = loop {
                    if let Some(shutdown) = &mut shutdown
                        && shutdown.try_recv().is_ok()
                    {
                        break 'listener;
                    }
                    if let Ok(event) = rx.try_recv() {
                        break event;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                };
                tx.send(event).ok();
            }
        });

        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        self.tx.subscribe()
    }

    fn get_blockchain_data_provider(&self) -> &Blockchain {
        &*self.blockchain
    }

    async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<Result<(), BlockchainError>, ChainInteractionError> {
        Ok(accept_transaction(&self.blockchain, &self.node_state, transaction).await)
    }

    async fn build_transaction(
        &self,
        sender: Private,
        receivers: Vec<(Public, u64)>,
    ) -> Result<Transaction, ChainInteractionError> {
        let mut tx = build_transaction(
            &*self.blockchain,
            sender,
            receivers,
            &*self.spent_inputs.read().await,
        )
        .await?;
        tx.compute_pow(
            &calculate_live_transaction_difficulty(
                &self.blockchain.get_transaction_difficulty(),
                self.node_state.mempool.mempool_size().await,
            ),
            Some(0.1),
        )
        .map_err(|e| BlockchainError::BincodeEncode(e.to_string()))?;

        self.spent_inputs
            .write()
            .await
            .extend_from_slice(&tx.inputs);

        Ok(tx)
    }

    async fn on_expiry(&self, transaction: Transaction) {
        let mut spent_inputs = self.spent_inputs.write().await;
        spent_inputs.retain(|input| !transaction.inputs.contains(input));
    }
}
