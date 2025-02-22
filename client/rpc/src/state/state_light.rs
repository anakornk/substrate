// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! State API backend for light nodes.

use std::{
	sync::Arc,
	collections::{HashSet, HashMap, hash_map::Entry},
};
use codec::Decode;
use futures::{
	future::{ready, Either},
	channel::oneshot::{channel, Sender},
	FutureExt, TryFutureExt,
	StreamExt as _, TryStreamExt as _,
};
use hash_db::Hasher;
use jsonrpc_pubsub::{typed::Subscriber, SubscriptionId};
use log::warn;
use parking_lot::Mutex;
use rpc::{
	Result as RpcResult,
	futures::Sink,
	futures::future::{result, Future},
	futures::stream::Stream,
};

use api::Subscriptions;
use client_api::backend::Backend;
use client::{
	BlockchainEvents, Client, CallExecutor, 
	error::Error as ClientError,
	light::{
		blockchain::{future_header, RemoteBlockchain},
		fetcher::{Fetcher, RemoteCallRequest, RemoteReadRequest, RemoteReadChildRequest},
	},
};
use primitives::{
	H256, Blake2Hasher, Bytes, OpaqueMetadata,
	storage::{StorageKey, StorageData, StorageChangeSet},
};
use runtime_version::RuntimeVersion;
use sr_primitives::{
	generic::BlockId,
	traits::Block as BlockT,
};

use super::{StateBackend, error::{FutureResult, Error}, client_err};

/// Storage data map of storage keys => (optional) storage value.
type StorageMap = HashMap<StorageKey, Option<StorageData>>;

/// State API backend for light nodes.
pub struct LightState<Block: BlockT, F: Fetcher<Block>, B, E, RA> {
	client: Arc<Client<B, E, Block, RA>>,
	subscriptions: Subscriptions,
	version_subscriptions: SimpleSubscriptions<Block::Hash, RuntimeVersion>,
	storage_subscriptions: Arc<Mutex<StorageSubscriptions<Block>>>,
	remote_blockchain: Arc<dyn RemoteBlockchain<Block>>,
	fetcher: Arc<F>,
}

/// Shared requests container.
trait SharedRequests<Hash, V>: Clone + Send + Sync {
	/// Tries to listen for already issued request, or issues request.
	///
	/// Returns true if requests has been issued.
	fn listen_request(
		&self,
		block: Hash,
		sender: Sender<Result<V, ()>>,
	) -> bool;

	/// Returns (and forgets) all listeners for given request.
	fn on_response_received(&self, block: Hash) -> Vec<Sender<Result<V, ()>>>;
}

/// Storage subscriptions data.
struct StorageSubscriptions<Block: BlockT> {
	/// Active storage requests.
	active_requests: HashMap<Block::Hash, Vec<Sender<Result<StorageMap, ()>>>>,
	/// Map of subscription => keys that this subscription watch for.
	keys_by_subscription: HashMap<SubscriptionId, HashSet<StorageKey>>,
	/// Map of key => set of subscriptions that watch this key.
	subscriptions_by_key: HashMap<StorageKey, HashSet<SubscriptionId>>,
}

impl<Block: BlockT> SharedRequests<Block::Hash, StorageMap> for Arc<Mutex<StorageSubscriptions<Block>>> {
	fn listen_request(
		&self,
		block: Block::Hash,
		sender: Sender<Result<StorageMap, ()>>,
	) -> bool {
		let mut subscriptions = self.lock();
		let active_requests_at = subscriptions.active_requests.entry(block).or_default();
		active_requests_at.push(sender);
		active_requests_at.len() == 1
	}

	fn on_response_received(&self, block: Block::Hash) -> Vec<Sender<Result<StorageMap, ()>>> {
		self.lock().active_requests.remove(&block).unwrap_or_default()
	}
}

/// Simple, maybe shared, subscription data that shares per block requests.
type SimpleSubscriptions<Hash, V> = Arc<Mutex<HashMap<Hash, Vec<Sender<Result<V, ()>>>>>>;

impl<Hash, V> SharedRequests<Hash, V> for SimpleSubscriptions<Hash, V> where
	Hash: Send + Eq + std::hash::Hash,
	V: Send,
{
	fn listen_request(
		&self,
		block: Hash,
		sender: Sender<Result<V, ()>>,
	) -> bool {
		let mut subscriptions = self.lock();
		let active_requests_at = subscriptions.entry(block).or_default();
		active_requests_at.push(sender);
		active_requests_at.len() == 1
	}

	fn on_response_received(&self, block: Hash) -> Vec<Sender<Result<V, ()>>> {
		self.lock().remove(&block).unwrap_or_default()
	}
}

impl<Block: BlockT, F: Fetcher<Block> + 'static, B, E, RA> LightState<Block, F, B, E, RA>
	where
		Block: BlockT<Hash=H256>,
		B: Backend<Block, Blake2Hasher> + Send + Sync + 'static,
		E: CallExecutor<Block, Blake2Hasher> + Send + Sync + 'static + Clone,
		RA: Send + Sync + 'static,
{
	/// Create new state API backend for light nodes.
	pub fn new(
		client: Arc<Client<B, E, Block, RA>>,
		subscriptions: Subscriptions,
		remote_blockchain: Arc<dyn RemoteBlockchain<Block>>,
		fetcher: Arc<F>,
	) -> Self {
		Self {
			client,
			subscriptions,
			version_subscriptions: Arc::new(Mutex::new(HashMap::new())),
			storage_subscriptions: Arc::new(Mutex::new(StorageSubscriptions {
				active_requests: HashMap::new(),
				keys_by_subscription: HashMap::new(),
				subscriptions_by_key: HashMap::new(),
			})),
			remote_blockchain,
			fetcher,
		}
	}

	/// Returns given block hash or best block hash if None is passed.
	fn block_or_best(&self, hash: Option<Block::Hash>) -> Block::Hash {
		hash.unwrap_or_else(|| self.client.info().chain.best_hash)
	}
}

impl<Block, F, B, E, RA> StateBackend<B, E, Block, RA> for LightState<Block, F, B, E, RA>
	where
		Block: BlockT<Hash=H256>,
		B: Backend<Block, Blake2Hasher> + Send + Sync + 'static,
		E: CallExecutor<Block, Blake2Hasher> + Send + Sync + 'static + Clone,
		RA: Send + Sync + 'static,
		F: Fetcher<Block> + 'static
{
	fn call(
		&self,
		block: Option<Block::Hash>,
		method: String,
		call_data: Bytes,
	) -> FutureResult<Bytes> {
		Box::new(call(
			&*self.remote_blockchain,
			self.fetcher.clone(),
			self.block_or_best(block),
			method,
			call_data,
		).boxed().compat())
	}

	fn storage_keys(
		&self,
		_block: Option<Block::Hash>,
		_prefix: StorageKey,
	) -> FutureResult<Vec<StorageKey>> {
		Box::new(result(Err(client_err(ClientError::NotAvailableOnLightClient))))
	}

	fn storage(
		&self,
		block: Option<Block::Hash>,
		key: StorageKey,
	) -> FutureResult<Option<StorageData>> {
		Box::new(storage(
			&*self.remote_blockchain,
			self.fetcher.clone(),
			self.block_or_best(block),
			vec![key.0.clone()],
		).boxed().compat().map(move |mut values| values
			.remove(&key)
			.expect("successful request has entries for all requested keys; qed")
		))
	}

	fn storage_hash(
		&self,
		block: Option<Block::Hash>,
		key: StorageKey,
	) -> FutureResult<Option<Block::Hash>> {
		Box::new(self
			.storage(block, key)
			.and_then(|maybe_storage|
				result(Ok(maybe_storage.map(|storage| Blake2Hasher::hash(&storage.0))))
			)
		)
	}

	fn child_storage_keys(
		&self,
		_block: Option<Block::Hash>,
		_child_storage_key: StorageKey,
		_prefix: StorageKey,
	) -> FutureResult<Vec<StorageKey>> {
		Box::new(result(Err(client_err(ClientError::NotAvailableOnLightClient))))
	}

	fn child_storage(
		&self,
		block: Option<Block::Hash>,
		child_storage_key: StorageKey,
		key: StorageKey,
	) -> FutureResult<Option<StorageData>> {
		let block = self.block_or_best(block);
		let fetcher = self.fetcher.clone();
		let child_storage = resolve_header(&*self.remote_blockchain, &*self.fetcher, block)
			.then(move |result| match result {
				Ok(header) => Either::Left(fetcher.remote_read_child(RemoteReadChildRequest {
					block,
					header,
					storage_key: child_storage_key.0,
					keys: vec![key.0.clone()],
					retry_count: Default::default(),
				}).then(move |result| ready(result
					.map(|mut data| data
						.remove(&key.0)
						.expect("successful result has entry for all keys; qed")
						.map(StorageData)
					)
					.map_err(client_err)
				))),
				Err(error) => Either::Right(ready(Err(error))),
			});

		Box::new(child_storage.boxed().compat())
	}

	fn child_storage_hash(
		&self,
		block: Option<Block::Hash>,
		child_storage_key: StorageKey,
		key: StorageKey,
	) -> FutureResult<Option<Block::Hash>> {
		Box::new(self
			.child_storage(block, child_storage_key, key)
			.and_then(|maybe_storage|
				result(Ok(maybe_storage.map(|storage| Blake2Hasher::hash(&storage.0))))
			)
		)
	}

	fn metadata(&self, block: Option<Block::Hash>) -> FutureResult<Bytes> {
		let metadata = self.call(block, "Metadata_metadata".into(), Bytes(Vec::new()))
			.and_then(|metadata| OpaqueMetadata::decode(&mut &metadata.0[..])
				.map(Into::into)
				.map_err(|decode_err| client_err(ClientError::CallResultDecode(
					"Unable to decode metadata",
					decode_err,
				))));

		Box::new(metadata)
	}

	fn runtime_version(&self, block: Option<Block::Hash>) -> FutureResult<RuntimeVersion> {
		Box::new(runtime_version(
			&*self.remote_blockchain,
			self.fetcher.clone(),
			self.block_or_best(block),
		).boxed().compat())
	}

	fn query_storage(
		&self,
		_from: Block::Hash,
		_to: Option<Block::Hash>,
		_keys: Vec<StorageKey>,
	) -> FutureResult<Vec<StorageChangeSet<Block::Hash>>> {
		Box::new(result(Err(client_err(ClientError::NotAvailableOnLightClient))))
	}

	fn subscribe_storage(
		&self,
		_meta: crate::metadata::Metadata,
		subscriber: Subscriber<StorageChangeSet<Block::Hash>>,
		keys: Option<Vec<StorageKey>>
	) {
		let keys = match keys {
			Some(keys) => keys,
			None => {
				warn!("Cannot subscribe to all keys on light client. Subscription rejected.");
				return;
			}
		};

		let keys = keys.iter().cloned().collect::<HashSet<_>>();
		let keys_to_check = keys.iter().map(|k| k.0.clone()).collect::<HashSet<_>>();
		let subscription_id = self.subscriptions.add(subscriber, move |sink| {
			let fetcher = self.fetcher.clone();
			let remote_blockchain = self.remote_blockchain.clone();
			let storage_subscriptions = self.storage_subscriptions.clone();
			let initial_block = self.block_or_best(None);
			let initial_keys = keys_to_check.iter().cloned().collect::<Vec<_>>();

			let changes_stream = subscription_stream::<Block, _, _, _, _, _, _, _, _>(
				storage_subscriptions.clone(),
				self.client
					.import_notification_stream()
					.map(|notification| Ok::<_, ()>(notification.hash))
					.compat(),
				display_error(storage(
					&*remote_blockchain,
					fetcher.clone(),
					initial_block,
					initial_keys,
				).map(move |r| r.map(|r| (initial_block, r)))),
				move |block| {
					// there'll be single request per block for all active subscriptions
					// with all subscribed keys
					let keys = storage_subscriptions
						.lock()
						.subscriptions_by_key
						.keys()
						.map(|k| k.0.clone())
						.collect();

					storage(
						&*remote_blockchain,
						fetcher.clone(),
						block,
						keys,
					)
				},
				move |block, old_value, new_value| {
					// let's only select keys which are valid for this subscription
					let new_value = new_value
						.iter()
						.filter(|(k, _)| keys_to_check.contains(&k.0))
						.map(|(k, v)| (k.clone(), v.clone()))
						.collect::<HashMap<_, _>>();
					let value_differs = old_value
						.as_ref()
						.map(|old_value| **old_value != new_value)
						.unwrap_or(true);
					match value_differs {
						true => Some(StorageChangeSet {
							block,
							changes: new_value
								.iter()
								.map(|(k, v)| (k.clone(), v.clone()))
								.collect(),
						}),
						false => None,
					}
				}
			);

			sink
				.sink_map_err(|e| warn!("Error sending notifications: {:?}", e))
				.send_all(changes_stream.map(|changes| Ok(changes)))
				// we ignore the resulting Stream (if the first stream is over we are unsubscribed)
				.map(|_| ())
		});

		// remember keys associated with this subscription
		let mut storage_subscriptions = self.storage_subscriptions.lock();
		storage_subscriptions.keys_by_subscription.insert(subscription_id.clone(), keys.clone());
		for key in keys {
			storage_subscriptions
				.subscriptions_by_key
				.entry(key)
				.or_default()
				.insert(subscription_id.clone());
		}
	}

	fn unsubscribe_storage(
		&self,
		_meta: Option<crate::metadata::Metadata>,
		id: SubscriptionId,
	) -> RpcResult<bool> {
		if !self.subscriptions.cancel(id.clone()) {
			return Ok(false);
		}

		// forget subscription keys
		let mut storage_subscriptions = self.storage_subscriptions.lock();
		let keys = storage_subscriptions.keys_by_subscription.remove(&id);
		for key in keys.into_iter().flat_map(|keys| keys.into_iter()) {
			match storage_subscriptions.subscriptions_by_key.entry(key) {
				Entry::Vacant(_) => unreachable!("every key from keys_by_subscription has\
					corresponding entry in subscriptions_by_key; qed"),
				Entry::Occupied(mut entry) => {
					entry.get_mut().remove(&id);
					if entry.get().is_empty() {
						entry.remove();
					}
				}
			}
		}

		Ok(true)
	}

	fn subscribe_runtime_version(
		&self,
		_meta: crate::metadata::Metadata,
		subscriber: Subscriber<RuntimeVersion>,
	) {
		self.subscriptions.add(subscriber, move |sink| {
			let fetcher = self.fetcher.clone();
			let remote_blockchain = self.remote_blockchain.clone();
			let version_subscriptions = self.version_subscriptions.clone();
			let initial_block = self.block_or_best(None);

			let versions_stream = subscription_stream::<Block, _, _, _, _, _, _, _, _>(
				version_subscriptions,
				self.client
					.import_notification_stream()
					.map(|notification| Ok::<_, ()>(notification.hash))
					.compat(),
				display_error(runtime_version(
					&*remote_blockchain,
					fetcher.clone(),
					initial_block,
				).map(move |r| r.map(|r| (initial_block, r)))),
				move |block| runtime_version(
					&*remote_blockchain,
					fetcher.clone(),
					block,
				),
				|_, old_version, new_version| {
					let version_differs = old_version
						.as_ref()
						.map(|old_version| *old_version != new_version)
						.unwrap_or(true);
					match version_differs {
						true => Some(new_version.clone()),
						false => None,
					}
				}
			);

			sink
				.sink_map_err(|e| warn!("Error sending notifications: {:?}", e))
				.send_all(versions_stream.map(|version| Ok(version)))
				// we ignore the resulting Stream (if the first stream is over we are unsubscribed)
				.map(|_| ())
		});
	}

	fn unsubscribe_runtime_version(
		&self,
		_meta: Option<crate::metadata::Metadata>,
		id: SubscriptionId,
	) -> RpcResult<bool> {
		Ok(self.subscriptions.cancel(id))
	}
}

/// Resolve header by hash.
fn resolve_header<Block: BlockT, F: Fetcher<Block>>(
	remote_blockchain: &dyn RemoteBlockchain<Block>,
	fetcher: &F,
	block: Block::Hash,
) -> impl std::future::Future<Output = Result<Block::Header, Error>> {
	let maybe_header = future_header(
		remote_blockchain,
		fetcher,
		BlockId::Hash(block),
	);

	maybe_header.then(move |result|
		ready(result.and_then(|maybe_header|
			maybe_header.ok_or(ClientError::UnknownBlock(format!("{}", block)))
		).map_err(client_err)),
	)
}

/// Call runtime method at given block
fn call<Block: BlockT, F: Fetcher<Block>>(
	remote_blockchain: &dyn RemoteBlockchain<Block>,
	fetcher: Arc<F>,
	block: Block::Hash,
	method: String,
	call_data: Bytes,
) -> impl std::future::Future<Output = Result<Bytes, Error>> {
	resolve_header(remote_blockchain, &*fetcher, block)
		.then(move |result| match result {
			Ok(header) => Either::Left(fetcher.remote_call(RemoteCallRequest {
				block,
				header,
				method,
				call_data: call_data.0,
				retry_count: Default::default(),
			}).then(|result| ready(result.map(Bytes).map_err(client_err)))),
			Err(error) => Either::Right(ready(Err(error))),
		})
}

/// Get runtime version at given block.
fn runtime_version<Block: BlockT, F: Fetcher<Block>>(
	remote_blockchain: &dyn RemoteBlockchain<Block>,
	fetcher: Arc<F>,
	block: Block::Hash,
) -> impl std::future::Future<Output = Result<RuntimeVersion, Error>> {
	call(
		remote_blockchain,
		fetcher,
		block,
		"Core_version".into(),
		Bytes(Vec::new()),
	)
	.then(|version| ready(version.and_then(|version|
		Decode::decode(&mut &version.0[..]).map_err(|_| client_err(ClientError::VersionInvalid))
	)))
}

/// Get storage value at given key at given block.
fn storage<Block: BlockT, F: Fetcher<Block>>(
	remote_blockchain: &dyn RemoteBlockchain<Block>,
	fetcher: Arc<F>,
	block: Block::Hash,
	keys: Vec<Vec<u8>>,
) -> impl std::future::Future<Output = Result<HashMap<StorageKey, Option<StorageData>>, Error>> {
	resolve_header(remote_blockchain, &*fetcher, block)
		.then(move |result| match result {
			Ok(header) => Either::Left(fetcher.remote_read(RemoteReadRequest {
				block,
				header,
				keys,
				retry_count: Default::default(),
			}).then(|result| ready(result
				.map(|result| result
					.into_iter()
					.map(|(key, value)| (StorageKey(key), value.map(StorageData)))
					.collect()
				).map_err(client_err)
			))),
			Err(error) => Either::Right(ready(Err(error))),
		})
}

/// Returns subscription stream that issues request on every imported block and
/// if value has changed from previous block, emits (stream) item.
fn subscription_stream<
	Block,
	Requests,
	FutureBlocksStream,
	V, N,
	InitialRequestFuture,
	IssueRequest, IssueRequestFuture,
	CompareValues,
>(
	shared_requests: Requests,
	future_blocks_stream: FutureBlocksStream,
	initial_request: InitialRequestFuture,
	issue_request: IssueRequest,
	compare_values: CompareValues,
) -> impl Stream<Item=N, Error=()> where
	Block: BlockT<Hash=H256>,
	Requests: 'static + SharedRequests<Block::Hash, V>,
	FutureBlocksStream: Stream<Item=Block::Hash, Error=()>,
	V: Send + 'static + Clone,
	InitialRequestFuture: std::future::Future<Output = Result<(Block::Hash, V), ()>> + Send + 'static,
	IssueRequest: 'static + Fn(Block::Hash) -> IssueRequestFuture,
	IssueRequestFuture: std::future::Future<Output = Result<V, Error>> + Send + 'static,
	CompareValues: Fn(Block::Hash, Option<&V>, &V) -> Option<N>,
{
	// we need to send initial value first, then we'll only be sending if value has changed
	let previous_value = Arc::new(Mutex::new(None));

	// prepare 'stream' of initial values
	let initial_value_stream = ignore_error(initial_request)
		.boxed()
		.compat()
		.into_stream();

	// prepare stream of future values
	//
	// we do not want to stop stream if single request fails
	// (the warning should have been already issued by the request issuer)
	let future_values_stream = future_blocks_stream
		.and_then(move |block| ignore_error(maybe_share_remote_request::<Block, _, _, _, _>(
			shared_requests.clone(),
			block,
			&issue_request,
		).map(move |r| r.map(|v| (block, v)))).boxed().compat());

	// now let's return changed values for selected blocks
	initial_value_stream
		.chain(future_values_stream)
		.filter_map(move |block_and_new_value| block_and_new_value.and_then(|(block, new_value)| {
			let mut previous_value = previous_value.lock();
			compare_values(block, previous_value.as_ref(), &new_value)
				.map(|notification_value| {
					*previous_value = Some(new_value);
					notification_value
				})
		}))
		.map_err(|_| ())
}

/// Request some data from remote node, probably reusing response from already
/// (in-progress) existing request.
fn maybe_share_remote_request<Block: BlockT, Requests, V, IssueRequest, IssueRequestFuture>(
	shared_requests: Requests,
	block: Block::Hash,
	issue_request: &IssueRequest,
) -> impl std::future::Future<Output = Result<V, ()>> where
	V: Clone,
	Requests: SharedRequests<Block::Hash, V>,
	IssueRequest: Fn(Block::Hash) -> IssueRequestFuture,
	IssueRequestFuture: std::future::Future<Output = Result<V, Error>>,
{
	let (sender, receiver) = channel();
	let need_issue_request = shared_requests.listen_request(block, sender);

	// if that isn't the first request - just listen for existing request' response
	if !need_issue_request {
		return Either::Right(receiver.then(|r| ready(r.unwrap_or(Err(())))));
	}

	// that is the first request - issue remote request + notify all listeners on
	// completion
	Either::Left(
		display_error(issue_request(block))
			.then(move |remote_result| {
				let listeners = shared_requests.on_response_received(block);
				// skip first element, because this future is the first element
				for receiver in listeners.into_iter().skip(1) {
					if let Err(_) = receiver.send(remote_result.clone()) {
						// we don't care if receiver has been dropped already
					}
				}

				ready(remote_result)
			})
	)
}

/// Convert successful future result into Ok(result) and error into Err(()),
/// displaying warning.
fn display_error<F, T>(future: F) -> impl std::future::Future<Output=Result<T, ()>> where
	F: std::future::Future<Output=Result<T, Error>>
{
	future.then(|result| ready(match result {
		Ok(result) => Ok(result),
		Err(err) => {
			warn!("Remote request for subscription data has failed with: {:?}", err);
			Err(())
		},
	}))
}

/// Convert successful future result into Ok(Some(result)) and error into Ok(None),
/// displaying warning.
fn ignore_error<F, T>(future: F) -> impl std::future::Future<Output=Result<Option<T>, ()>> where
	F: std::future::Future<Output=Result<T, ()>>
{
	future.then(|result| ready(match result {
		Ok(result) => Ok(Some(result)),
		Err(()) => Ok(None),
	}))	
}

#[cfg(test)]
mod tests {
	use rpc::futures::stream::futures_ordered;
	use test_client::runtime::Block;
	use super::*;

	#[test]
	fn subscription_stream_works() {
		let stream = subscription_stream::<Block, _, _, _, _, _, _, _, _>(
			SimpleSubscriptions::default(),
			futures_ordered(vec![result(Ok(H256::from([2; 32]))), result(Ok(H256::from([3; 32])))]),
			ready(Ok((H256::from([1; 32]), 100))),
			|block| match block[0] {
				2 => ready(Ok(100)),
				3 => ready(Ok(200)),
				_ => unreachable!("should not issue additional requests"),
			},
			|_, old_value, new_value| match old_value == Some(new_value) {
				true => None,
				false => Some(new_value.clone()),
			}
		);

		assert_eq!(
			stream.collect().wait(),
			Ok(vec![100, 200])
		);
	}

	#[test]
	fn subscription_stream_ignores_failed_requests() {
		let stream = subscription_stream::<Block, _, _, _, _, _, _, _, _>(
			SimpleSubscriptions::default(),
			futures_ordered(vec![result(Ok(H256::from([2; 32]))), result(Ok(H256::from([3; 32])))]),
			ready(Ok((H256::from([1; 32]), 100))),
			|block| match block[0] {
				2 => ready(Err(client_err(ClientError::NotAvailableOnLightClient))),
				3 => ready(Ok(200)),
				_ => unreachable!("should not issue additional requests"),
			},
			|_, old_value, new_value| match old_value == Some(new_value) {
				true => None,
				false => Some(new_value.clone()),
			}
		);

		assert_eq!(
			stream.collect().wait(),
			Ok(vec![100, 200])
		);
	}

	#[test]
	fn maybe_share_remote_request_shares_request() {
		type UnreachableFuture = futures::future::Ready<Result<u32, Error>>;

		let shared_requests = SimpleSubscriptions::default();

		// let's 'issue' requests for B1
		shared_requests.lock().insert(
			H256::from([1; 32]),
			vec![channel().0],
		);

		// make sure that no additional requests are issued when we're asking for B1
		let _ = maybe_share_remote_request::<Block, _, _, _, UnreachableFuture>(
			shared_requests.clone(),
			H256::from([1; 32]),
			&|_| unreachable!("no duplicate requests issued"),
		);

		// make sure that additional requests is issued when we're asking for B2
		let request_issued = Arc::new(Mutex::new(false));
		let _ = maybe_share_remote_request::<Block, _, _, _, UnreachableFuture>(
			shared_requests.clone(),
			H256::from([2; 32]),
			&|_| {
				*request_issued.lock() = true;
				ready(Ok(Default::default()))
			},
		);
		assert!(*request_issued.lock());
	}
}
