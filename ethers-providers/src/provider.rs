use crate::{
    ens,
    pubsub::{PubsubClient, SubscriptionStream},
    stream::{FilterWatcher, DEFAULT_POLL_INTERVAL},
    FromErr, Http as HttpProvider, JsonRpcClient, MockProvider, PendingTransaction,
};

use ethers_core::{
    abi::{self, Detokenize, ParamType},
    types::{
        Address, Block, BlockId, BlockNumber, BlockTrace, Bytes, Filter, Log, NameOrAddress,
        Selector, Signature, Trace, TraceFilter, TraceType, Transaction, TransactionReceipt,
        TransactionRequest, TxHash, TxpoolContent, TxpoolInspect, TxpoolStatus, H256, U256, U64,
    },
    utils,
};

use crate::Middleware;
use async_trait::async_trait;
use hex::FromHex;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use url::{ParseError, Url};

use std::{convert::TryFrom, fmt::Debug, time::Duration};
use tracing::trace;
use tracing_futures::Instrument;

/// An abstract provider for interacting with the [Ethereum JSON RPC
/// API](https://github.com/ethereum/wiki/wiki/JSON-RPC). Must be instantiated
/// with a data transport which implements the [`JsonRpcClient`](trait@crate::JsonRpcClient) trait
/// (e.g. [HTTP](crate::Http), Websockets etc.)
///
/// # Example
///
/// ```no_run
/// # async fn foo() -> Result<(), Box<dyn std::error::Error>> {
/// use ethers::providers::{Middleware, Provider, Http};
/// use std::convert::TryFrom;
///
/// let provider = Provider::<Http>::try_from(
///     "https://mainnet.infura.io/v3/c60b0bb42f8a4c6481ecd229eddaca27"
/// ).expect("could not instantiate HTTP Provider");
///
/// let block = provider.get_block(100u64).await?;
/// println!("Got block: {}", serde_json::to_string(&block)?);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
// TODO: Convert to proper struct
pub struct Provider<P>(P, Option<Address>, Option<Duration>, Option<Address>);

impl<P> AsRef<P> for Provider<P> {
    fn as_ref(&self) -> &P {
        &self.0
    }
}

impl FromErr<ProviderError> for ProviderError {
    fn from(src: ProviderError) -> Self {
        src
    }
}

#[derive(Debug, Error)]
/// An error thrown when making a call to the provider
pub enum ProviderError {
    /// An internal error in the JSON RPC Client
    #[error(transparent)]
    JsonRpcClientError(#[from] Box<dyn std::error::Error + Send + Sync>),

    /// An error during ENS name resolution
    #[error("ens name not found: {0}")]
    EnsError(String),

    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    #[error(transparent)]
    HexError(#[from] hex::FromHexError),

    #[error("custom error: {0}")]
    CustomError(String),
}

/// Types of filters supported by the JSON-RPC.
#[derive(Clone, Debug)]
pub enum FilterKind<'a> {
    /// `eth_newBlockFilter`
    Logs(&'a Filter),

    /// `eth_newBlockFilter` filter
    NewBlocks,

    /// `eth_newPendingTransactionFilter` filter
    PendingTransactions,
}

// JSON RPC bindings
impl<P: JsonRpcClient> Provider<P> {
    /// Instantiate a new provider with a backend.
    pub fn new(provider: P) -> Self {
        Self(provider, None, None, None)
    }

    pub fn with_sender(mut self, address: impl Into<Address>) -> Self {
        self.3 = Some(address.into());
        self
    }

    async fn request<T, R>(&self, method: &str, params: T) -> Result<R, ProviderError>
    where
        T: Debug + Serialize + Send + Sync,
        R: Serialize + DeserializeOwned + Debug,
    {
        let span =
            tracing::trace_span!("rpc", method = method, params = ?serde_json::to_string(&params)?);
        // https://docs.rs/tracing/0.1.22/tracing/span/struct.Span.html#in-asynchronous-code
        let res = async move {
            trace!("tx");
            let res: R = self.0.request(method, params).await.map_err(Into::into)?;
            trace!(rx = ?serde_json::to_string(&res)?);
            Ok::<_, ProviderError>(res)
        }
        .instrument(span)
        .await?;
        Ok(res)
    }

    async fn get_block_gen<Tx: Default + Serialize + DeserializeOwned + Debug>(
        &self,
        id: BlockId,
        include_txs: bool,
    ) -> Result<Option<Block<Tx>>, ProviderError> {
        let include_txs = utils::serialize(&include_txs);

        Ok(match id {
            BlockId::Hash(hash) => {
                let hash = utils::serialize(&hash);
                self.request("eth_getBlockByHash", [hash, include_txs])
                    .await?
            }
            BlockId::Number(num) => {
                let num = utils::serialize(&num);
                self.request("eth_getBlockByNumber", [num, include_txs])
                    .await?
            }
        })
    }
}

#[async_trait]
impl<P: JsonRpcClient> Middleware for Provider<P> {
    type Error = ProviderError;
    type Provider = P;
    type Inner = Self;

    fn inner(&self) -> &Self::Inner {
        unreachable!("There is no inner provider here")
    }

    fn provider(&self) -> &Provider<Self::Provider> {
        self
    }

    ////// Blockchain Status
    //
    // Functions for querying the state of the blockchain

    /// Gets the latest block number via the `eth_BlockNumber` API
    async fn get_block_number(&self) -> Result<U64, ProviderError> {
        self.request("eth_blockNumber", ()).await
    }

    /// Gets the block at `block_hash_or_number` (transaction hashes only)
    async fn get_block<T: Into<BlockId> + Send + Sync>(
        &self,
        block_hash_or_number: T,
    ) -> Result<Option<Block<TxHash>>, Self::Error> {
        self.get_block_gen(block_hash_or_number.into(), false).await
    }

    /// Gets the block at `block_hash_or_number` (full transactions included)
    async fn get_block_with_txs<T: Into<BlockId> + Send + Sync>(
        &self,
        block_hash_or_number: T,
    ) -> Result<Option<Block<Transaction>>, ProviderError> {
        self.get_block_gen(block_hash_or_number.into(), true).await
    }

    /// Gets the transaction with `transaction_hash`
    async fn get_transaction<T: Send + Sync + Into<TxHash>>(
        &self,
        transaction_hash: T,
    ) -> Result<Option<Transaction>, ProviderError> {
        let hash = transaction_hash.into();
        self.request("eth_getTransactionByHash", [hash]).await
    }

    /// Gets the transaction receipt with `transaction_hash`
    async fn get_transaction_receipt<T: Send + Sync + Into<TxHash>>(
        &self,
        transaction_hash: T,
    ) -> Result<Option<TransactionReceipt>, ProviderError> {
        let hash = transaction_hash.into();
        self.request("eth_getTransactionReceipt", [hash]).await
    }

    /// Gets the current gas price as estimated by the node
    async fn get_gas_price(&self) -> Result<U256, ProviderError> {
        self.request("eth_gasPrice", ()).await
    }

    /// Gets the accounts on the node
    async fn get_accounts(&self) -> Result<Vec<Address>, ProviderError> {
        self.request("eth_accounts", ()).await
    }

    /// Returns the nonce of the address
    async fn get_transaction_count<T: Into<NameOrAddress> + Send + Sync>(
        &self,
        from: T,
        block: Option<BlockId>,
    ) -> Result<U256, ProviderError> {
        let from = match from.into() {
            NameOrAddress::Name(ens_name) => self.resolve_name(&ens_name).await?,
            NameOrAddress::Address(addr) => addr,
        };

        let from = utils::serialize(&from);
        let block = utils::serialize(&block.unwrap_or_else(|| BlockNumber::Latest.into()));
        self.request("eth_getTransactionCount", [from, block]).await
    }

    /// Returns the account's balance
    async fn get_balance<T: Into<NameOrAddress> + Send + Sync>(
        &self,
        from: T,
        block: Option<BlockId>,
    ) -> Result<U256, ProviderError> {
        let from = match from.into() {
            NameOrAddress::Name(ens_name) => self.resolve_name(&ens_name).await?,
            NameOrAddress::Address(addr) => addr,
        };

        let from = utils::serialize(&from);
        let block = utils::serialize(&block.unwrap_or_else(|| BlockNumber::Latest.into()));
        self.request("eth_getBalance", [from, block]).await
    }

    /// Returns the currently configured chain id, a value used in replay-protected
    /// transaction signing as introduced by EIP-155.
    async fn get_chainid(&self) -> Result<U256, ProviderError> {
        self.request("eth_chainId", ()).await
    }

    ////// Contract Execution
    //
    // These are relatively low-level calls. The Contracts API should usually be used instead.

    /// Sends the read-only (constant) transaction to a single Ethereum node and return the result (as bytes) of executing it.
    /// This is free, since it does not change any state on the blockchain.
    async fn call(
        &self,
        tx: &TransactionRequest,
        block: Option<BlockId>,
    ) -> Result<Bytes, ProviderError> {
        let tx = utils::serialize(tx);
        let block = utils::serialize(&block.unwrap_or_else(|| BlockNumber::Latest.into()));
        self.request("eth_call", [tx, block]).await
    }

    /// Sends a transaction to a single Ethereum node and return the estimated amount of gas required (as a U256) to send it
    /// This is free, but only an estimate. Providing too little gas will result in a transaction being rejected
    /// (while still consuming all provided gas).
    async fn estimate_gas(&self, tx: &TransactionRequest) -> Result<U256, ProviderError> {
        self.request("eth_estimateGas", [tx]).await
    }

    /// Sends the transaction to the entire Ethereum network and returns the transaction's hash
    /// This will consume gas from the account that signed the transaction.
    async fn send_transaction(
        &self,
        mut tx: TransactionRequest,
        _: Option<BlockId>,
    ) -> Result<PendingTransaction<'_, P>, ProviderError> {
        if tx.from.is_none() {
            tx.from = self.3;
        }

        if tx.gas.is_none() {
            tx.gas = Some(self.estimate_gas(&tx).await?);
        }

        if let Some(NameOrAddress::Name(ref ens_name)) = tx.to {
            // resolve to an address
            let addr = self.resolve_name(&ens_name).await?;

            // set the value
            tx.to = Some(addr.into())
        }

        let tx_hash = self.request("eth_sendTransaction", [tx]).await?;

        Ok(PendingTransaction::new(tx_hash, self).interval(self.get_interval()))
    }

    /// Send the raw RLP encoded transaction to the entire Ethereum network and returns the transaction's hash
    /// This will consume gas from the account that signed the transaction.
    async fn send_raw_transaction<'a>(
        &'a self,
        tx: &Transaction,
    ) -> Result<PendingTransaction<'a, P>, ProviderError> {
        let rlp = utils::serialize(&tx.rlp());
        let tx_hash = self.request("eth_sendRawTransaction", [rlp]).await?;
        Ok(PendingTransaction::new(tx_hash, self).interval(self.get_interval()))
    }

    /// The JSON-RPC provider is at the bottom-most position in the middleware stack. Here we check
    /// if it has the key for the sender address unlocked, as well as supports the `eth_sign` call.
    async fn is_signer(&self) -> bool {
        match self.3 {
            Some(sender) => self.sign(vec![], &sender).await.is_ok(),
            None => false,
        }
    }

    /// Signs data using a specific account. This account needs to be unlocked.
    async fn sign<T: Into<Bytes> + Send + Sync>(
        &self,
        data: T,
        from: &Address,
    ) -> Result<Signature, ProviderError> {
        let data = utils::serialize(&data.into());
        let from = utils::serialize(from);

        // get the response from `eth_sign` call and trim the 0x-prefix if present.
        let sig: String = self.request("eth_sign", [from, data]).await?;
        let sig = sig.strip_prefix("0x").unwrap_or(&sig);

        // decode the signature.
        let sig = hex::decode(sig)?;
        Ok(Signature::try_from(sig.as_slice())
            .map_err(|e| ProviderError::CustomError(e.to_string()))?)
    }

    ////// Contract state

    /// Returns an array (possibly empty) of logs that match the filter
    async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>, ProviderError> {
        self.request("eth_getLogs", [filter]).await
    }

    /// Streams matching filter logs
    async fn watch<'a>(
        &'a self,
        filter: &Filter,
    ) -> Result<FilterWatcher<'a, P, Log>, ProviderError> {
        let id = self.new_filter(FilterKind::Logs(filter)).await?;
        let filter = FilterWatcher::new(id, self).interval(self.get_interval());
        Ok(filter)
    }

    /// Streams new block hashes
    async fn watch_blocks(&self) -> Result<FilterWatcher<'_, P, H256>, ProviderError> {
        let id = self.new_filter(FilterKind::NewBlocks).await?;
        let filter = FilterWatcher::new(id, self).interval(self.get_interval());
        Ok(filter)
    }

    /// Streams pending transactions
    async fn watch_pending_transactions(
        &self,
    ) -> Result<FilterWatcher<'_, P, H256>, ProviderError> {
        let id = self.new_filter(FilterKind::PendingTransactions).await?;
        let filter = FilterWatcher::new(id, self).interval(self.get_interval());
        Ok(filter)
    }

    /// Creates a filter object, based on filter options, to notify when the state changes (logs).
    /// To check if the state has changed, call `get_filter_changes` with the filter id.
    async fn new_filter(&self, filter: FilterKind<'_>) -> Result<U256, ProviderError> {
        let (method, args) = match filter {
            FilterKind::NewBlocks => ("eth_newBlockFilter", vec![]),
            FilterKind::PendingTransactions => ("eth_newPendingTransactionFilter", vec![]),
            FilterKind::Logs(filter) => ("eth_newFilter", vec![utils::serialize(&filter)]),
        };

        self.request(method, args).await
    }

    /// Uninstalls a filter
    async fn uninstall_filter<T: Into<U256> + Send + Sync>(
        &self,
        id: T,
    ) -> Result<bool, ProviderError> {
        let id = utils::serialize(&id.into());
        self.request("eth_uninstallFilter", [id]).await
    }

    /// Polling method for a filter, which returns an array of logs which occurred since last poll.
    ///
    /// This method must be called with one of the following return types, depending on the filter
    /// type:
    /// - `eth_newBlockFilter`: [`H256`], returns block hashes
    /// - `eth_newPendingTransactionFilter`: [`H256`], returns transaction hashes
    /// - `eth_newFilter`: [`Log`], returns raw logs
    ///
    /// If one of these types is not used, decoding will fail and the method will
    /// return an error.
    ///
    /// [`H256`]: ethers_core::types::H256
    /// [`Log`]: ethers_core::types::Log
    async fn get_filter_changes<T, R>(&self, id: T) -> Result<Vec<R>, ProviderError>
    where
        T: Into<U256> + Send + Sync,
        R: Serialize + DeserializeOwned + Send + Sync + Debug,
    {
        let id = utils::serialize(&id.into());
        self.request("eth_getFilterChanges", [id]).await
    }

    /// Get the storage of an address for a particular slot location
    async fn get_storage_at<T: Into<NameOrAddress> + Send + Sync>(
        &self,
        from: T,
        location: H256,
        block: Option<BlockId>,
    ) -> Result<H256, ProviderError> {
        let from = match from.into() {
            NameOrAddress::Name(ens_name) => self.resolve_name(&ens_name).await?,
            NameOrAddress::Address(addr) => addr,
        };

        let from = utils::serialize(&from);
        let location = utils::serialize(&location);
        let block = utils::serialize(&block.unwrap_or_else(|| BlockNumber::Latest.into()));

        // get the hex encoded value.
        let value: String = self
            .request("eth_getStorageAt", [from, location, block])
            .await?;
        // get rid of the 0x prefix and left pad it with zeroes.
        let value = format!("{:0>64}", value.replace("0x", ""));
        Ok(H256::from_slice(&Vec::from_hex(value)?))
    }

    /// Returns the deployed code at a given address
    async fn get_code<T: Into<NameOrAddress> + Send + Sync>(
        &self,
        at: T,
        block: Option<BlockId>,
    ) -> Result<Bytes, ProviderError> {
        let at = match at.into() {
            NameOrAddress::Name(ens_name) => self.resolve_name(&ens_name).await?,
            NameOrAddress::Address(addr) => addr,
        };

        let at = utils::serialize(&at);
        let block = utils::serialize(&block.unwrap_or_else(|| BlockNumber::Latest.into()));
        self.request("eth_getCode", [at, block]).await
    }

    ////// Ethereum Naming Service
    // The Ethereum Naming Service (ENS) allows easy to remember and use names to
    // be assigned to Ethereum addresses. Any provider operation which takes an address
    // may also take an ENS name.
    //
    // ENS also provides the ability for a reverse lookup, which determines the name for an address if it has been configured.

    /// Returns the address that the `ens_name` resolves to (or None if not configured).
    ///
    /// # Panics
    ///
    /// If the bytes returned from the ENS registrar/resolver cannot be interpreted as
    /// an address. This should theoretically never happen.
    async fn resolve_name(&self, ens_name: &str) -> Result<Address, ProviderError> {
        self.query_resolver(ParamType::Address, ens_name, ens::ADDR_SELECTOR)
            .await
    }

    /// Returns the ENS name the `address` resolves to (or None if not configured).
    /// # Panics
    ///
    /// If the bytes returned from the ENS registrar/resolver cannot be interpreted as
    /// a string. This should theoretically never happen.
    async fn lookup_address(&self, address: Address) -> Result<String, ProviderError> {
        let ens_name = ens::reverse_address(address);
        self.query_resolver(ParamType::String, &ens_name, ens::NAME_SELECTOR)
            .await
    }

    /// Returns the details of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    /// Ref: [Here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_content)
    async fn txpool_content(&self) -> Result<TxpoolContent, ProviderError> {
        self.request("txpool_content", ()).await
    }

    /// Returns a summary of all the transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    /// Ref: [Here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_inspect)
    async fn txpool_inspect(&self) -> Result<TxpoolInspect, ProviderError> {
        self.request("txpool_inspect", ()).await
    }

    /// Returns the number of transactions currently pending for inclusion in the next block(s), as
    /// well as the ones that are being scheduled for future execution only.
    /// Ref: [Here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_status)
    async fn txpool_status(&self) -> Result<TxpoolStatus, ProviderError> {
        self.request("txpool_status", ()).await
    }

    /// Executes the given call and returns a number of possible traces for it
    async fn trace_call(
        &self,
        req: TransactionRequest,
        trace_type: Vec<TraceType>,
        block: Option<BlockNumber>,
    ) -> Result<BlockTrace, ProviderError> {
        let req = utils::serialize(&req);
        let block = utils::serialize(&block.unwrap_or(BlockNumber::Latest));
        let trace_type = utils::serialize(&trace_type);
        self.request("trace_call", [req, trace_type, block]).await
    }

    /// Traces a call to `eth_sendRawTransaction` without making the call, returning the traces
    async fn trace_raw_transaction(
        &self,
        data: Bytes,
        trace_type: Vec<TraceType>,
    ) -> Result<BlockTrace, ProviderError> {
        let data = utils::serialize(&data);
        let trace_type = utils::serialize(&trace_type);
        self.request("trace_rawTransaction", [data, trace_type])
            .await
    }

    /// Replays a transaction, returning the traces
    async fn trace_replay_transaction(
        &self,
        hash: H256,
        trace_type: Vec<TraceType>,
    ) -> Result<BlockTrace, ProviderError> {
        let hash = utils::serialize(&hash);
        let trace_type = utils::serialize(&trace_type);
        self.request("trace_replayTransaction", [hash, trace_type])
            .await
    }

    /// Replays all transactions in a block returning the requested traces for each transaction
    async fn trace_replay_block_transactions(
        &self,
        block: BlockNumber,
        trace_type: Vec<TraceType>,
    ) -> Result<Vec<BlockTrace>, ProviderError> {
        let block = utils::serialize(&block);
        let trace_type = utils::serialize(&trace_type);
        self.request("trace_replayBlockTransactions", [block, trace_type])
            .await
    }

    /// Returns traces created at given block
    async fn trace_block(&self, block: BlockNumber) -> Result<Vec<Trace>, ProviderError> {
        let block = utils::serialize(&block);
        self.request("trace_block", [block]).await
    }

    /// Return traces matching the given filter
    async fn trace_filter(&self, filter: TraceFilter) -> Result<Vec<Trace>, ProviderError> {
        let filter = utils::serialize(&filter);
        self.request("trace_filter", vec![filter]).await
    }

    /// Returns trace at the given position
    async fn trace_get<T: Into<U64> + Send + Sync>(
        &self,
        hash: H256,
        index: Vec<T>,
    ) -> Result<Trace, ProviderError> {
        let hash = utils::serialize(&hash);
        let index: Vec<U64> = index.into_iter().map(|i| i.into()).collect();
        let index = utils::serialize(&index);
        self.request("trace_get", vec![hash, index]).await
    }

    /// Returns all traces of a given transaction
    async fn trace_transaction(&self, hash: H256) -> Result<Vec<Trace>, ProviderError> {
        let hash = utils::serialize(&hash);
        self.request("trace_transaction", vec![hash]).await
    }

    /// Returns all receipts for that block. Must be done on a parity node.
    async fn parity_block_receipts<T: Into<BlockNumber> + Send + Sync>(
        &self,
        block: T,
    ) -> Result<Vec<TransactionReceipt>, Self::Error> {
        self.request("parity_getBlockReceipts", vec![block.into()])
            .await
    }

    async fn subscribe<T, R>(
        &self,
        params: T,
    ) -> Result<SubscriptionStream<'_, P, R>, ProviderError>
    where
        T: Debug + Serialize + Send + Sync,
        R: DeserializeOwned + Send + Sync,
        P: PubsubClient,
    {
        let id: U256 = self.request("eth_subscribe", params).await?;
        SubscriptionStream::new(id, self).map_err(Into::into)
    }

    async fn unsubscribe<T>(&self, id: T) -> Result<bool, ProviderError>
    where
        T: Into<U256> + Send + Sync,
        P: PubsubClient,
    {
        self.request("eth_unsubscribe", [id.into()]).await
    }

    async fn subscribe_blocks(
        &self,
    ) -> Result<SubscriptionStream<'_, P, Block<TxHash>>, ProviderError>
    where
        P: PubsubClient,
    {
        self.subscribe(["newHeads"]).await
    }

    async fn subscribe_pending_txs(
        &self,
    ) -> Result<SubscriptionStream<'_, P, TxHash>, ProviderError>
    where
        P: PubsubClient,
    {
        self.subscribe(["newPendingTransactions"]).await
    }

    async fn subscribe_logs<'a>(
        &'a self,
        filter: &Filter,
    ) -> Result<SubscriptionStream<'a, P, Log>, ProviderError>
    where
        P: PubsubClient,
    {
        let logs = utils::serialize(&"logs"); // TODO: Make this a static
        let filter = utils::serialize(filter);
        self.subscribe([logs, filter]).await
    }
}

impl<P: JsonRpcClient> Provider<P> {
    async fn query_resolver<T: Detokenize>(
        &self,
        param: ParamType,
        ens_name: &str,
        selector: Selector,
    ) -> Result<T, ProviderError> {
        // Get the ENS address, prioritize the local override variable
        let ens_addr = self.1.unwrap_or(ens::ENS_ADDRESS);

        // first get the resolver responsible for this name
        // the call will return a Bytes array which we convert to an address
        let data = self
            .call(&ens::get_resolver(ens_addr, ens_name), None)
            .await?;

        let resolver_address: Address = decode_bytes(ParamType::Address, data);
        if resolver_address == Address::zero() {
            return Err(ProviderError::EnsError(ens_name.to_owned()));
        }

        // resolve
        let data = self
            .call(&ens::resolve(resolver_address, selector, ens_name), None)
            .await?;

        Ok(decode_bytes(param, data))
    }

    #[cfg(test)]
    /// ganache-only function for mining empty blocks
    pub async fn mine(&self, num_blocks: usize) -> Result<(), ProviderError> {
        for _ in 0..num_blocks {
            self.0
                .request::<_, U256>("evm_mine", None::<()>)
                .await
                .map_err(Into::into)?;
        }
        Ok(())
    }

    /// Sets the ENS Address (default: mainnet)
    pub fn ens<T: Into<Address>>(mut self, ens: T) -> Self {
        self.1 = Some(ens.into());
        self
    }

    /// Sets the default polling interval for event filters and pending transactions
    /// (default: 7 seconds)
    pub fn interval<T: Into<Duration>>(mut self, interval: T) -> Self {
        self.2 = Some(interval.into());
        self
    }

    /// Gets the polling interval which the provider currently uses for event filters
    /// and pending transactions (default: 7 seconds)
    pub fn get_interval(&self) -> Duration {
        self.2.unwrap_or(DEFAULT_POLL_INTERVAL)
    }
}

#[cfg(feature = "ws")]
impl Provider<crate::Ws> {
    /// Direct connection to a websocket endpoint
    pub async fn connect(
        url: impl tokio_tungstenite::tungstenite::client::IntoClientRequest + Unpin,
    ) -> Result<Self, ProviderError> {
        let ws = crate::Ws::connect(url).await?;
        Ok(Self::new(ws))
    }
}

impl Provider<MockProvider> {
    /// Returns a `Provider` instantiated with an internal "mock" transport.
    ///
    /// # Example
    ///
    /// ```
    /// # async fn foo() -> Result<(), Box<dyn std::error::Error>> {
    /// use ethers::{types::U64, providers::{Middleware, Provider}};
    /// // Instantiate the provider
    /// let (provider, mock) = Provider::mocked();
    /// // Push the mock response
    /// mock.push(U64::from(12))?;
    /// // Make the call
    /// let blk = provider.get_block_number().await.unwrap();
    /// // The response matches
    /// assert_eq!(blk.as_u64(), 12);
    /// // and the request as well!
    /// mock.assert_request("eth_blockNumber", ()).unwrap();
    /// # Ok(())
    /// # }
    /// ```
    pub fn mocked() -> (Self, MockProvider) {
        let mock = MockProvider::new();
        let mock_clone = mock.clone();
        (Self::new(mock), mock_clone)
    }
}

/// infallible conversion of Bytes to Address/String
///
/// # Panics
///
/// If the provided bytes were not an interpretation of an address
fn decode_bytes<T: Detokenize>(param: ParamType, bytes: Bytes) -> T {
    let tokens = abi::decode(&[param], &bytes.as_ref())
        .expect("could not abi-decode bytes to address tokens");
    T::from_tokens(tokens).expect("could not parse tokens as address")
}

impl TryFrom<&str> for Provider<HttpProvider> {
    type Error = ParseError;

    fn try_from(src: &str) -> Result<Self, Self::Error> {
        Ok(Provider(
            HttpProvider::new(Url::parse(src)?),
            None,
            None,
            None,
        ))
    }
}

impl TryFrom<String> for Provider<HttpProvider> {
    type Error = ParseError;

    fn try_from(src: String) -> Result<Self, Self::Error> {
        Provider::try_from(src.as_str())
    }
}

#[cfg(test)]
mod ens_tests {
    use super::*;

    const INFURA: &str = "https://mainnet.infura.io/v3/c60b0bb42f8a4c6481ecd229eddaca27";

    #[tokio::test]
    // Test vector from: https://docs.ethers.io/ethers.js/v5-beta/api-providers.html#id2
    async fn mainnet_resolve_name() {
        let provider = Provider::<HttpProvider>::try_from(INFURA).unwrap();

        let addr = provider
            .resolve_name("registrar.firefly.eth")
            .await
            .unwrap();
        assert_eq!(
            addr,
            "6fC21092DA55B392b045eD78F4732bff3C580e2c".parse().unwrap()
        );

        // registrar not found
        provider.resolve_name("asdfasdffads").await.unwrap_err();

        // name not found
        provider
            .resolve_name("asdfasdf.registrar.firefly.eth")
            .await
            .unwrap_err();
    }

    #[tokio::test]
    // Test vector from: https://docs.ethers.io/ethers.js/v5-beta/api-providers.html#id2
    async fn mainnet_lookup_address() {
        let provider = Provider::<HttpProvider>::try_from(INFURA).unwrap();

        let name = provider
            .lookup_address("6fC21092DA55B392b045eD78F4732bff3C580e2c".parse().unwrap())
            .await
            .unwrap();

        assert_eq!(name, "registrar.firefly.eth");

        provider
            .lookup_address("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".parse().unwrap())
            .await
            .unwrap_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Http;
    use ethers_core::types::H256;
    use futures_util::StreamExt;

    #[tokio::test]
    #[ignore]
    // Ganache new block filters are super buggy! This test must be run with
    // geth or parity running e.g. `geth --dev --rpc --dev.period 1`
    async fn test_new_block_filter() {
        let num_blocks = 3;

        let provider = Provider::<HttpProvider>::try_from("http://localhost:8545")
            .unwrap()
            .interval(Duration::from_millis(1000));
        let start_block = provider.get_block_number().await.unwrap();

        let stream = provider.watch_blocks().await.unwrap().stream();

        let hashes: Vec<H256> = stream.take(num_blocks).collect::<Vec<H256>>().await;
        for (i, hash) in hashes.iter().enumerate() {
            let block = provider
                .get_block(start_block + i as u64 + 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(*hash, block.hash.unwrap());
        }
    }

    #[tokio::test]
    async fn test_is_signer() {
        use ethers_core::utils::Ganache;
        use std::str::FromStr;

        let ganache = Ganache::new().spawn();
        let provider = Provider::<Http>::try_from(ganache.endpoint())
            .unwrap()
            .with_sender(ganache.addresses()[0]);
        assert_eq!(provider.is_signer().await, true);

        let provider = Provider::<Http>::try_from(ganache.endpoint()).unwrap();
        assert_eq!(provider.is_signer().await, false);

        let sender = Address::from_str("635B4764D1939DfAcD3a8014726159abC277BecC")
            .expect("should be able to parse hex address");
        let provider = Provider::<Http>::try_from(
            "https://ropsten.infura.io/v3/fd8b88b56aa84f6da87b60f5441d6778",
        )
        .unwrap()
        .with_sender(sender);
        assert_eq!(provider.is_signer().await, false);
    }

    // this must be run with geth or parity since ganache-core still does not support
    // eth_pendingTransactions, https://github.com/trufflesuite/ganache-core/issues/405
    // example command: `geth --dev --rpc --dev.period 1`
    #[tokio::test]
    #[ignore]
    async fn test_new_pending_txs_filter() {
        let num_txs = 5;

        let provider = Provider::<HttpProvider>::try_from("http://localhost:8545")
            .unwrap()
            .interval(Duration::from_millis(1000));
        let accounts = provider.get_accounts().await.unwrap();

        let stream = provider
            .watch_pending_transactions()
            .await
            .unwrap()
            .stream();

        let mut tx_hashes = Vec::new();
        let tx = TransactionRequest::new()
            .from(accounts[0])
            .to(accounts[0])
            .value(1e18 as u64);

        for _ in 0..num_txs {
            tx_hashes.push(provider.send_transaction(tx.clone(), None).await.unwrap());
        }

        let hashes: Vec<H256> = stream.take(num_txs).collect::<Vec<H256>>().await;
        assert_eq!(tx_hashes, hashes);
    }

    #[tokio::test]
    async fn receipt_on_unmined_tx() {
        use ethers_core::{
            types::TransactionRequest,
            utils::{parse_ether, Ganache},
        };
        let ganache = Ganache::new().block_time(2u64).spawn();
        let provider = Provider::<Http>::try_from(ganache.endpoint()).unwrap();

        let accounts = provider.get_accounts().await.unwrap();
        let tx = TransactionRequest::pay(accounts[0], parse_ether(1u64).unwrap()).from(accounts[0]);
        let pending_tx = provider.send_transaction(tx, None).await.unwrap();

        assert!(provider
            .get_transaction_receipt(*pending_tx)
            .await
            .unwrap()
            .is_none());

        let hash = *pending_tx;
        let receipt = pending_tx.await.unwrap();
        assert_eq!(receipt.transaction_hash, hash);
    }

    #[tokio::test]
    async fn parity_block_receipts() {
        let url = match std::env::var("PARITY") {
            Ok(inner) => inner,
            _ => return,
        };
        let provider = Provider::<Http>::try_from(url.as_str()).unwrap();
        let receipts = provider.parity_block_receipts(10657200).await.unwrap();
        assert!(!receipts.is_empty());
    }

    #[tokio::test]
    // Celo blocks can not get parsed when used with Ganache
    #[cfg(not(feature = "celo"))]
    async fn block_subscribe() {
        use ethers_core::utils::Ganache;
        use futures_util::StreamExt;
        let ganache = Ganache::new().block_time(2u64).spawn();
        let provider = Provider::connect(ganache.ws_endpoint()).await.unwrap();

        let stream = provider.subscribe_blocks().await.unwrap();
        let blocks = stream
            .take(3)
            .map(|x| x.number.unwrap().as_u64())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(blocks, vec![1, 2, 3]);
    }
}
