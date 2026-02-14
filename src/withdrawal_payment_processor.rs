use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use snap_coin::{
    core::{
        block::Block,
        transaction::{Transaction, TransactionId},
    },
    crypto::keys::{Private, Public},
    full_node::node_state::ChainEvent,
};
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use crate::chain_interaction::{ChainInteraction, ChainInteractionError};

/// Stable identifier for a withdrawal that persists across resubmissions
pub type WithdrawalId = Uuid;

/// Status of any withdrawal transaction
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WithdrawalStatus {
    /// Waiting in mempool
    Pending { transaction: TransactionId },
    /// Added to an unconfirmed block
    Confirming { transaction: TransactionId },
    /// Expired in mempool (being resubmitted)
    Expired { transaction: TransactionId },
    /// Confirmed with enough blocks on top
    Confirmed { transaction: TransactionId },
}

/// Withdrawal transaction data
#[derive(Clone)]
struct WithdrawalData {
    sender_private: Private,
    recipients: Vec<(Public, u64)>,
    status: WithdrawalStatus,
    current_tx_id: TransactionId,
    current_tx: Transaction,
    is_resubmitting: bool,
}

/// Handles all outgoing withdrawals
pub struct WithdrawalPaymentProcessor<T: ChainInteraction> {
    chain_interface: T,
    withdrawal_by_id: RwLock<HashMap<WithdrawalId, WithdrawalData>>,
    tx_id_to_withdrawal_id: RwLock<HashMap<TransactionId, WithdrawalId>>,
    block_confirmation_queue: RwLock<VecDeque<Block>>,
    shutdown_tx: RwLock<Option<broadcast::Sender<()>>>,
}

/// Handles custom user-defined logic on withdrawal confirmation
#[async_trait]
pub trait OnWithdrawalConfirmation: Send + Sync + Clone + 'static {
    async fn on_confirmation(&self, withdrawal_id: WithdrawalId, transaction: Transaction);
}

impl<T: ChainInteraction> WithdrawalPaymentProcessor<T> {
    pub fn new(chain_interface: T) -> Arc<Self> {
        Arc::new(Self {
            chain_interface,
            withdrawal_by_id: RwLock::new(HashMap::new()),
            tx_id_to_withdrawal_id: RwLock::new(HashMap::new()),
            block_confirmation_queue: RwLock::new(VecDeque::new()),
            shutdown_tx: RwLock::new(None),
        })
    }

    /// Start the payment processor loop
    pub async fn start(
        self: &Arc<Self>,
        confirmation_count: usize,
        on_confirmation: impl OnWithdrawalConfirmation,
    ) -> Result<(), ChainInteractionError> {
        let (s_tx, _s_rx) = broadcast::channel(1);

        self.chain_interface
            .start_listener(Some(s_tx.subscribe()))
            .await?;
        let mut on_event = self.chain_interface.subscribe();

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
                        let mut withdrawal_by_id = self_clone.withdrawal_by_id.write().await;
                        let tx_to_wd = self_clone.tx_id_to_withdrawal_id.read().await;

                        // Mark transactions as Confirming
                        for tx in &block.transactions {
                            if let Some(tx_id) = tx.transaction_id {
                                if let Some(wd_id) = tx_to_wd.get(&tx_id) {
                                    if let Some(data) = withdrawal_by_id.get_mut(wd_id) {
                                        if matches!(data.status, WithdrawalStatus::Pending { .. }) {
                                            data.status = WithdrawalStatus::Confirming {
                                                transaction: tx_id,
                                            };
                                        }
                                    }
                                }
                            }
                        }

                        drop(tx_to_wd);

                        // Handle reorg
                        while let Some(queue_block) = queue.back() {
                            if block.meta.previous_block != queue_block.meta.hash.unwrap() {
                                let tx_to_wd = self_clone.tx_id_to_withdrawal_id.read().await;
                                // A reorg happened, revert transactions back to pending
                                for tx in &queue_block.transactions {
                                    if let Some(tx_id) = tx.transaction_id {
                                        if let Some(wd_id) = tx_to_wd.get(&tx_id) {
                                            if let Some(data) = withdrawal_by_id.get_mut(wd_id) {
                                                if matches!(data.status, WithdrawalStatus::Confirming { .. }) {
                                                    data.status = WithdrawalStatus::Pending {
                                                        transaction: tx_id,
                                                    };
                                                }
                                            }
                                        }
                                    }
                                }
                                drop(tx_to_wd);
                                queue.pop_back();
                            } else {
                                break;
                            }
                        }

                        queue.push_back(block.clone());

                        // Confirm after enough blocks
                        if queue.len() > confirmation_count {
                            let confirmed_block = queue.pop_front().unwrap();
                            let tx_to_wd = self_clone.tx_id_to_withdrawal_id.read().await;

                            for tx in &confirmed_block.transactions {
                                if let Some(tx_id) = tx.transaction_id {
                                    if let Some(wd_id) = tx_to_wd.get(&tx_id) {
                                        if let Some(data) = withdrawal_by_id.get_mut(wd_id) {
                                            if matches!(data.status, WithdrawalStatus::Confirming { .. }) {
                                                data.status = WithdrawalStatus::Confirmed {
                                                    transaction: tx_id,
                                                };
                                                let tx = tx.clone();
                                                let wd_id = *wd_id;
                                                let on_confirmation = on_confirmation.clone();
                                                tokio::spawn(async move {
                                                    on_confirmation.on_confirmation(wd_id, tx).await;
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    ChainEvent::Transaction { .. } => {
                        // Transaction entered mempool - already tracked
                    }

                    ChainEvent::TransactionExpiration { transaction } => {
                        // Check if this withdrawal is already being resubmitted
                        let should_resubmit = {
                            let tx_to_wd = self_clone.tx_id_to_withdrawal_id.read().await;
                            let mut withdrawal_by_id = self_clone.withdrawal_by_id.write().await;

                            if let Some(wd_id) = tx_to_wd.get(&transaction) {
                                if let Some(data) = withdrawal_by_id.get_mut(wd_id) {
                                    if matches!(data.status, WithdrawalStatus::Pending { .. }) && !data.is_resubmitting {
                                        data.status = WithdrawalStatus::Expired { transaction };
                                        data.is_resubmitting = true;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if !should_resubmit {
                            continue;
                        }

                        // Get withdrawal data including the expired transaction
                        let (withdrawal_id, sender_private, recipients, expired_tx) = {
                            let tx_to_wd = self_clone.tx_id_to_withdrawal_id.read().await;
                            let withdrawal_by_id = self_clone.withdrawal_by_id.read().await;
                            
                            let wd_id = tx_to_wd.get(&transaction).copied().unwrap();
                            let data = withdrawal_by_id.get(&wd_id).unwrap();
                            
                            (wd_id, data.sender_private.clone(), data.recipients.clone(), data.current_tx.clone())
                        };

                        // Call on_expiry with the actual transaction
                        self_clone.chain_interface.on_expiry(expired_tx).await;

                        // Build and submit new transaction
                        let result = async {
                            let tx = self_clone
                                .chain_interface
                                .build_transaction(sender_private, recipients.clone())
                                .await?;

                            let new_tx_id = tx.transaction_id.ok_or_else(|| {
                                ChainInteractionError::MissingTransactionId
                            })?;

                            self_clone
                                .chain_interface
                                .submit_transaction(tx.clone())
                                .await?
                                .map_err(|e| ChainInteractionError::SubmissionFailed(e.to_string()))?;

                            Ok::<_, ChainInteractionError>((new_tx_id, tx))
                        }
                        .await;

                        // Update tracking
                        let mut withdrawal_by_id = self_clone.withdrawal_by_id.write().await;
                        let mut tx_to_wd = self_clone.tx_id_to_withdrawal_id.write().await;

                        if let Some(data) = withdrawal_by_id.get_mut(&withdrawal_id) {
                            match result {
                                Ok((new_tx_id, new_tx)) => {
                                    tx_to_wd.remove(&transaction);
                                    data.current_tx_id = new_tx_id;
                                    data.current_tx = new_tx;
                                    data.status = WithdrawalStatus::Pending {
                                        transaction: new_tx_id,
                                    };
                                    data.is_resubmitting = false;
                                    tx_to_wd.insert(new_tx_id, withdrawal_id);
                                }
                                Err(_) => {
                                    data.is_resubmitting = false;
                                }
                            }
                        }
                    }
                }
            }
        });

        *self.shutdown_tx.write().await = Some(s_tx);
        Ok(())
    }

    /// Submit a new withdrawal transaction
    pub async fn submit_withdrawal(
        &self,
        recipients: Vec<(Public, u64)>,
        sender_private: Private,
    ) -> Result<WithdrawalId, ChainInteractionError> {
        // Build the transaction
        let transaction = self
            .chain_interface
            .build_transaction(sender_private, recipients.clone())
            .await?;

        let tx_id = transaction.transaction_id.ok_or_else(|| {
            ChainInteractionError::MissingTransactionId
        })?;
        let withdrawal_id = Uuid::new_v4();

        // Submit the transaction
        self.chain_interface
            .submit_transaction(transaction.clone())
            .await?
            .map_err(|e| ChainInteractionError::SubmissionFailed(e.to_string()))?;

        // Track the withdrawal
        self.withdrawal_by_id.write().await.insert(
            withdrawal_id,
            WithdrawalData {
                sender_private,
                recipients,
                status: WithdrawalStatus::Pending {
                    transaction: tx_id,
                },
                current_tx_id: tx_id,
                current_tx: transaction,
                is_resubmitting: false,
            },
        );

        self.tx_id_to_withdrawal_id
            .write()
            .await
            .insert(tx_id, withdrawal_id);

        Ok(withdrawal_id)
    }

    /// Stop tracking a withdrawal (e.g., after it's confirmed and processed)
    pub async fn untrack_withdrawal(&self, withdrawal_id: WithdrawalId) {
        if let Some(data) = self.withdrawal_by_id.write().await.remove(&withdrawal_id) {
            self.tx_id_to_withdrawal_id
                .write()
                .await
                .remove(&data.current_tx_id);
        }
    }

    /// Get the status of a withdrawal
    pub async fn get_withdrawal_status(
        &self,
        withdrawal_id: WithdrawalId,
    ) -> Option<WithdrawalStatus> {
        self.withdrawal_by_id
            .read()
            .await
            .get(&withdrawal_id)
            .map(|data| data.status)
    }

    /// Get the current transaction ID for a withdrawal
    pub async fn get_current_tx_id(
        &self,
        withdrawal_id: WithdrawalId,
    ) -> Option<TransactionId> {
        self.withdrawal_by_id
            .read()
            .await
            .get(&withdrawal_id)
            .map(|data| data.current_tx_id)
    }

    /// Get the current transaction for a withdrawal
    pub async fn get_current_tx(
        &self,
        withdrawal_id: WithdrawalId,
    ) -> Option<Transaction> {
        self.withdrawal_by_id
            .read()
            .await
            .get(&withdrawal_id)
            .map(|data| data.current_tx.clone())
    }

    /// Get all tracked withdrawals and their statuses
    pub async fn get_all_withdrawals(&self) -> HashMap<WithdrawalId, WithdrawalStatus> {
        self.withdrawal_by_id
            .read()
            .await
            .iter()
            .map(|(id, data)| (*id, data.status))
            .collect()
    }

    pub async fn stop(&self) {
        if let Some(tx) = self.shutdown_tx.read().await.as_ref() {
            tx.send(()).ok();
        }
    }
}