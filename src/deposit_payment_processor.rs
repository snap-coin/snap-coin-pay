use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use snap_coin::{
    core::{
        block::Block,
        transaction::{Transaction, TransactionId},
    },
    crypto::keys::Public,
    full_node::node_state::ChainEvent,
};
use tokio::sync::{RwLock, broadcast};

use crate::chain_interaction::{ChainInteraction, ChainInteractionError};

/// Status of any deposit address
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DepositStatus {
    /// Waiting in mempool
    Pending { transaction: TransactionId },
    /// Added to a un-confirmed block
    Confirming { transaction: TransactionId },
    /// Expired in mempool
    Expired { transaction: TransactionId },
    /// Not received by the node at all
    NotSeen,
    /// Confirmed
    Confirmed { transaction: TransactionId },
}

/// Handles all incoming deposits, addresses etc
pub struct DepositPaymentProcessor {
    deposit_addresses: RwLock<HashSet<Public>>,
    deposit_statuses: RwLock<HashMap<Public, DepositStatus>>,
    block_confirmation_queue: RwLock<VecDeque<Block>>,
    shutdown_tx: RwLock<Option<broadcast::Sender<()>>>,
}

/// Handles custom user-defined on transaction confirmation logic
#[async_trait]
pub trait OnConfirmation: Send + Sync + Clone + 'static {
    async fn on_confirmation(&self, deposit_address: Public, transaction: Transaction);
}

impl DepositPaymentProcessor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            deposit_addresses: RwLock::new(HashSet::new()),
            deposit_statuses: RwLock::new(HashMap::new()),
            block_confirmation_queue: RwLock::new(VecDeque::new()),
            shutdown_tx: RwLock::new(None),
        })
    }

    /// Start the payment processor loop
    pub async fn start(
        self: &Arc<Self>,
        chain_update_listener: impl ChainInteraction,
        confirmation_count: usize,
        on_confirmation: impl OnConfirmation,
    ) -> Result<(), ChainInteractionError> {
        let (s_tx, _s_rx) = broadcast::channel(1);

        chain_update_listener
            .start_listener(Some(s_tx.subscribe()))
            .await?;
        let mut on_event = chain_update_listener.subscribe();

        let mut s_rx = s_tx.subscribe();
        let self_clone = self.clone();

        tokio::spawn(async move {
            'listener: loop {
                let event = loop {
                    if s_rx.try_recv().is_ok() {
                        break 'listener;
                    }
                    if let Ok(event) = on_event.try_recv() {
                        break event;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                };

                match event {
                    ChainEvent::Block { block } => {
                        let mut queue = self_clone.block_confirmation_queue.write().await;
                        let deposit_addresses = self_clone.deposit_addresses.read().await;

                        // Mark transactions as Confirming
                        for tx in &block.transactions {
                            for receiver in &tx.outputs {
                                if deposit_addresses.contains(&receiver.receiver) {
                                    self_clone.deposit_statuses.write().await.insert(
                                        receiver.receiver,
                                        DepositStatus::Confirming {
                                            transaction: tx.transaction_id.unwrap(),
                                        },
                                    );
                                }
                            }
                        }

                        // Handle reorg
                        while let Some(queue_block) = queue.back() {
                            if block.meta.previous_block != queue_block.meta.hash.unwrap() {
                                for tx in &queue_block.transactions {
                                    for output in &tx.outputs {
                                        if deposit_addresses.contains(&output.receiver) {
                                            // A transaction was reorged, remove it form status
                                            self_clone
                                                .deposit_statuses
                                                .write()
                                                .await
                                                .remove(&output.receiver);
                                        }
                                    }
                                }
                                queue.pop_back();
                            } else {
                                break;
                            }
                        }

                        queue.push_back(block.clone());

                        // Confirm after enough blocks
                        if queue.len() > confirmation_count {
                            let confirmed_block = queue.pop_front().unwrap();

                            for tx in &confirmed_block.transactions {
                                for receiver in &tx.outputs {
                                    if deposit_addresses.contains(&receiver.receiver) {
                                        self_clone.deposit_statuses.write().await.insert(
                                            receiver.receiver,
                                            DepositStatus::Confirmed {
                                                transaction: tx.transaction_id.unwrap(),
                                            },
                                        );
                                        let tx = tx.clone();
                                        let on_confirmation = on_confirmation.clone();
                                        let receiver = receiver.receiver;
                                        tokio::spawn(async move {
                                            on_confirmation.on_confirmation(receiver, tx).await;
                                        });
                                    }
                                }
                            }
                        }
                    }

                    ChainEvent::Transaction { transaction } => {
                        for receiver in &transaction.outputs {
                            if self_clone
                                .deposit_addresses
                                .read()
                                .await
                                .contains(&receiver.receiver)
                            {
                                self_clone.deposit_statuses.write().await.insert(
                                    receiver.receiver,
                                    DepositStatus::Pending {
                                        transaction: transaction.transaction_id.unwrap(),
                                    },
                                );
                            }
                        }
                    }

                    ChainEvent::TransactionExpiration { transaction } => {
                        for (_, status) in self_clone.deposit_statuses.write().await.iter_mut() {
                            if let DepositStatus::Pending {
                                transaction: pending_tx,
                            } = status
                                && *pending_tx == transaction
                            {
                                *status = DepositStatus::Expired { transaction };
                            }
                        }
                    }
                }
            }
        });

        *self.shutdown_tx.write().await = Some(s_tx);
        Ok(())
    }

    /// Add a deposit address this listener cares about
    pub async fn add_deposit_address(&self, address: Public) {
        self.deposit_addresses.write().await.insert(address);
        self.deposit_statuses
            .write()
            .await
            .insert(address, DepositStatus::NotSeen);
    }

    /// Remove a deposit address this listener cares about
    pub async fn remove_deposit_address(&self, address: Public) {
        self.deposit_addresses.write().await.remove(&address);
        self.deposit_statuses.write().await.remove(&address);
    }

    /// Get the status of a deposit address this listener cares about
    pub async fn get_deposit_status(&self, address: Public) -> DepositStatus {
        *self
            .deposit_statuses
            .read()
            .await
            .get(&address)
            .unwrap_or(&DepositStatus::NotSeen)
    }

    pub async fn stop(&self) {
        if let Some(tx) = self.shutdown_tx.read().await.as_ref() {
            tx.send(()).ok();
        }
    }
}
