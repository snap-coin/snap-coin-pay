/// Provides chain updates
pub mod chain_interaction;
/// Handles incoming (deposit) payments
pub mod deposit_payment_processor;
/// Handles outgoing (withdrawal) payments
pub mod withdrawal_payment_processor;

#[cfg(test)]
pub mod test {
    use std::time::Duration;

    use async_trait::async_trait;
    use snap_coin::{
        core::transaction::Transaction,
        crypto::keys::{Private, Public},
    };

    use crate::{
        chain_interaction::ApiChainInteraction,
        deposit_payment_processor::{DepositPaymentProcessor, OnConfirmation},
        withdrawal_payment_processor::{
            OnWithdrawalConfirmation, WithdrawalId, WithdrawalPaymentProcessor,
        },
    };

    #[derive(Clone)]
    struct ConfirmationHandler {}

    #[async_trait]
    impl OnConfirmation for ConfirmationHandler {
        async fn on_confirmation(&self, _deposit_address: Public, transaction: Transaction) {
            println!("Confirmed deposit tx! {:#?}", transaction);
        }
    }

    #[derive(Clone)]
    struct WithdrawalConfirmationHandler {}

    #[async_trait]
    impl OnWithdrawalConfirmation for WithdrawalConfirmationHandler {
        async fn on_confirmation(&self, withdrawal_id: WithdrawalId, transaction: Transaction) {
            println!(
                "Confirmed withdraw ({}) tx! {:#?}",
                withdrawal_id, transaction
            );
        }
    }

    #[tokio::test]
    async fn deposit() -> anyhow::Result<()> {
        let chain_interaction_interface =
            ApiChainInteraction::new("127.0.0.1:3003".parse().unwrap()).await?;
        let on_confirmation = ConfirmationHandler {};

        let deposit_processor = DepositPaymentProcessor::new();
        deposit_processor
            .start(chain_interaction_interface, 3, on_confirmation)
            .await?;

        let deposit_address = Private::new_random().to_public();
        println!("Deposit address: {}", deposit_address.dump_base36());
        deposit_processor.add_deposit_address(deposit_address).await;
        println!("Waiting for deposit...");

        loop {
            println!(
                "Deposit status: {:?}",
                deposit_processor.get_deposit_status(deposit_address).await
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        #[allow(unreachable_code)]
        Ok(())
    }

    #[tokio::test]
    async fn withdrawal() -> anyhow::Result<()> {
        let chain_interaction_interface =
            ApiChainInteraction::new("127.0.0.1:3003".parse().unwrap()).await?;
        let on_confirmation = WithdrawalConfirmationHandler {};

        let withdrawal_processor = WithdrawalPaymentProcessor::new(chain_interaction_interface);
        withdrawal_processor.start(3, on_confirmation).await?;

        let wallet_private =
            Private::new_from_base36("jw4iipqhquxz60pw3b5vt531fy5z58w9zu56qfm631adq71pz").unwrap();
        let receiver =
            Public::new_from_base36("2j25rf3xur2ytvb8yg31l6h61t6944gfshufypdlzyqp6jdm08").unwrap();

        println!("Starting withdrawal...");
        let wid = withdrawal_processor
            .submit_withdrawal(vec![(receiver, 1)], wallet_private)
            .await?;
        let wid2 = withdrawal_processor
            .submit_withdrawal(vec![(receiver, 1)], wallet_private)
            .await?;

        loop {
            println!(
                "Withdraw status: ({wid}) {:?}",
                withdrawal_processor.get_withdrawal_status(wid).await
            );
            println!(
                "Withdraw status 2: ({wid2}) {:?}",
                withdrawal_processor.get_withdrawal_status(wid2).await
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        #[allow(unreachable_code)]
        Ok(())
    }
}
