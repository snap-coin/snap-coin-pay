# Snap Coin Pay
A payment management library to make deposits and withdrawals easy on the **Snap Coin Network!**

## General
You probably also want to add the `snap-coin`, `async-trait`, and `tokio` libraries, since these are quite essential.

## Usage
We must have *some* a connection to the Snap Coin Network, and there are two standardized ways to do this, and to create a `ChainInteraction` struct, that the deposit processor will use:
- Snap Coin API Client
```rust
use snap_coin_pay::chain_interaction::ApiChainInteraction;

let chain_interaction = ApiChainInteraction::new("127.0.0.1:3003".parse().unwrap());
```
This line connects to a Snap Coin Node over the Snap Coin API (`snap_coin::api`) protocol on port `3003`. This approach is great when you need more then one instance of this program running.
- Snap Coin Embedded Node, when we need maximum performance, and uptime.
```rust
use snap_coin::full_node::{SharedBlockchain, create_full_node, node_state::SharedNodeState};
use snap_coin_pay::chain_interaction::NodeChainInteraction;

let (blockchain, node_state): (SharedBlockchain, SharedNodeState) = create_full_node("./node-snap-coin-pay", false);

let chain_interaction = NodeChainInteraction::new(node_state.clone(), shared_blockchain.clone());
```
This line connects to a Snap Coin Node over the Snap Coin API protocol, port `3003`. This approach is great when you need more then one instance of this program running.

### Deposits
Handling (*incoming*) deposit transactions can be hard, since many factors like re-orgs, expiration, etc. can be daunting to implement, so Snap Coin Pay exposes the `deposit_payment_processor` module that handles it all for you!
We must also define on custom on deposit confirmation logic.
```rust
use snap_coin_pay::deposit_payment_processor::DepositPaymentProcessor;

// We define deposit confirmation logic
#[derive(Clone)]
struct ConfirmationHandler {}
#[async_trait]
impl OnConfirmation for ConfirmationHandler {
    // Gets called when a deposit is confirmed
    async fn on_confirmation(&self, deposit_address: Public, transaction: Transaction) {
        println!("Confirmed deposit! Transaction:\n{:#?}", transaction);
        // You would normally define some sort of actual logic here
    }
}

// We create a instance of the confirmation ConfirmationHandler
let on_confirmation = ConfirmationHandler {};

// We create a deposit processor
let deposit_processor = DepositProcessor::new();

// We start the deposit processor (20 is the amount of confirmation required for a transaction to be confirmed. Depending on your application 10-20 is fine)
deposit_processor
    .start(chain_interaction, 10, on_confirmation)
    .await?;
```
Accepting deposits is easy, we just need to add a deposit address, and the deposit processor will automatically start listening for any deposits to it, and it will provide you with a deposit status.
```rust
let deposit_address = Private::new_random().to_public(); // Create some deposit address, in a real application you want to store this somewhere safe

println!("Deposit address: {}", deposit_address.dump_base36());
// We tell the deposit processor to listen for events regarding this address. This does not persist between program runs - You need to re-add all deposit addresses every time the program starts
deposit_processor.add_deposit_address(deposit_address).await;
println!("Waiting for deposit...");

loop {
    /// We can check the deposit status at any time with just the deposit address. This returns a DepositStatus struct that also contains a transaction id, if the 
    println!(
        "Deposit status: {:?}",
        deposit_processor.get_deposit_status(deposit_address).await
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
}
```
The `DepositProcessor` struct has a few extra functions:
- `processor.stop()` Stop the processor, and all it's underlying tasks.
- `processor.remove_deposit_address(deposit_address: Public)` Stop listening for deposits to this address.

### Withdrawals
Snap Coin Pay also supports atomic withdrawals, meaning you can withdraw funds, without having to worry about forks and expiry. It is very similar to the `DepositProcessor`, and utilizes similar parameters.

```rust
#[derive(Clone)]
struct WithdrawalConfirmationHandler {}

#[async_trait]
impl OnWithdrawalConfirmation for WithdrawalConfirmationHandler {
    async fn on_confirmation(&self, withdrawal_id: WithdrawalId, transaction: Transaction) {
        println!(
            "Confirmed withdraw ({})! {:#?}",
            withdrawal_id, transaction
        );
    }
}

let on_confirmation = WithdrawalConfirmationHandler {};

let withdrawal_processor = WithdrawalPaymentProcessor::new(chain_interaction_interface);

// We start the withdrawal processor (20 is the amount of confirmation required for a transaction to be confirmed. Depending on your application 10-20 is fine)
withdrawal_processor.start(10, on_confirmation).await?; // Amount

let wallet_private =
    Private::new_from_base36("jw4iipqhquxz60pw3b5vt531fy5z58w9zu56qfm631adq71pz").unwrap();
let receiver =
    Public::new_from_base36("2j25rf3xur2ytvb8yg31l6h61t6944gfshufypdlzyqp6jdm08").unwrap();

println!("Starting withdrawal...");
let withdrawal_id = withdrawal_processor // You receive a withdrawal ID (type = UUID), which you can use to get it's status at any time
    .submit_withdrawal(vec![(receiver, 1)], wallet_private) // 1 is amount of NANO (like sat to BTC, but for SNAP) to withdraw.
    .await?;

loop {
    println!(
        "Withdraw status: ({withdrawal_id}) {:?}",
        withdrawal_processor.get_withdrawal_status(withdrawal_id).await
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
}
```