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

//! Execution extensions for runtime calls.
//!
//! This module is responsible for defining the execution
//! strategy for the runtime calls and provide the right `Externalities`
//! extensions to support APIs for particular execution context & capabilities.

use std::sync::{Weak, Arc};
use codec::Decode;
use primitives::{
	ExecutionContext,
	offchain::{self, OffchainExt, TransactionPoolExt},
	traits::{BareCryptoStorePtr, KeystoreExt},
};
use sr_primitives::{
	generic::BlockId,
	traits,
	offchain::{TransactionPool},
};
use state_machine::{ExecutionStrategy, ExecutionManager, DefaultHandler};
use externalities::Extensions;
use parking_lot::RwLock;

/// Execution strategies settings.
#[derive(Debug, Clone)]
pub struct ExecutionStrategies {
	/// Execution strategy used when syncing.
	pub syncing: ExecutionStrategy,
	/// Execution strategy used when importing blocks.
	pub importing: ExecutionStrategy,
	/// Execution strategy used when constructing blocks.
	pub block_construction: ExecutionStrategy,
	/// Execution strategy used for offchain workers.
	pub offchain_worker: ExecutionStrategy,
	/// Execution strategy used in other cases.
	pub other: ExecutionStrategy,
}

impl Default for ExecutionStrategies {
	fn default() -> ExecutionStrategies {
		ExecutionStrategies {
			syncing: ExecutionStrategy::NativeElseWasm,
			importing: ExecutionStrategy::NativeElseWasm,
			block_construction: ExecutionStrategy::AlwaysWasm,
			offchain_worker: ExecutionStrategy::NativeWhenPossible,
			other: ExecutionStrategy::NativeElseWasm,
		}
	}
}

/// A producer of execution extensions for offchain calls.
///
/// This crate aggregates extensions available for the offchain calls
/// and is responsbile to produce a right `Extensions` object
/// for each call, based on required `Capabilities`.
pub struct ExecutionExtensions<Block: traits::Block> {
	strategies: ExecutionStrategies,
	keystore: Option<BareCryptoStorePtr>,
	transaction_pool: RwLock<Option<Weak<dyn TransactionPool<Block>>>>,
}

impl<Block: traits::Block> Default for ExecutionExtensions<Block> {
	fn default() -> Self {
		Self {
			strategies: Default::default(),
			keystore: None,
			transaction_pool: RwLock::new(None),
		}
	}
}

impl<Block: traits::Block> ExecutionExtensions<Block> {
	/// Create new `ExecutionExtensions` given a `keystore` and `ExecutionStrategies`.
	pub fn new(
		strategies: ExecutionStrategies,
		keystore: Option<BareCryptoStorePtr>,
	) -> Self {
		let transaction_pool = RwLock::new(None);
		Self { strategies, keystore, transaction_pool }
	}

	/// Get a reference to the execution strategies.
	pub fn strategies(&self) -> &ExecutionStrategies {
		&self.strategies
	}

	/// Register transaction pool extension.
	///
	/// To break retain cycle between `Client` and `TransactionPool` we require this
	/// extension to be a `Weak` reference.
	/// That's also the reason why it's being registered lazily instead of
	/// during initialisation.
	pub fn register_transaction_pool(&self, pool: Weak<dyn TransactionPool<Block>>) {
		*self.transaction_pool.write() = Some(pool);
	}

	/// Create `ExecutionManager` and `Extensions` for given offchain call.
	///
	/// Based on the execution context and capabilities it produces
	/// the right manager and extensions object to support desired set of APIs.
	pub fn manager_and_extensions<E: std::fmt::Debug, R: codec::Codec>(
		&self,
		at: &BlockId<Block>,
		context: ExecutionContext,
	) -> (
		ExecutionManager<DefaultHandler<R, E>>,
		Extensions,
	) {
		let manager = match context {
			ExecutionContext::BlockConstruction =>
				self.strategies.block_construction.get_manager(),
			ExecutionContext::Syncing =>
				self.strategies.syncing.get_manager(),
			ExecutionContext::Importing =>
				self.strategies.importing.get_manager(),
			ExecutionContext::OffchainCall(Some((_, capabilities))) if capabilities.has_all() =>
				self.strategies.offchain_worker.get_manager(),
			ExecutionContext::OffchainCall(_) =>
				self.strategies.other.get_manager(),
		};

		let capabilities = context.capabilities();

		let mut extensions = Extensions::new();

		if capabilities.has(offchain::Capability::Keystore) {
			if let Some(keystore) = self.keystore.as_ref() {
				extensions.register(KeystoreExt(keystore.clone()));
			}
		}

		if capabilities.has(offchain::Capability::TransactionPool) {
			if let Some(pool) = self.transaction_pool.read().as_ref().and_then(|x| x.upgrade()) {
				extensions.register(TransactionPoolExt(Box::new(TransactionPoolAdapter {
					at: *at,
					pool,
				}) as _));
			}
		}

		if let ExecutionContext::OffchainCall(Some(ext)) = context {
			extensions.register(
				OffchainExt::new(offchain::LimitedExternalities::new(capabilities, ext.0))
			)
		}

		(manager, extensions)
	}
}

/// A wrapper type to pass `BlockId` to the actual transaction pool.
struct TransactionPoolAdapter<Block: traits::Block> {
	at: BlockId<Block>,
	pool: Arc<dyn TransactionPool<Block>>,
}

impl<Block: traits::Block> offchain::TransactionPool for TransactionPoolAdapter<Block> {
	fn submit_transaction(&mut self, data: Vec<u8>) -> Result<(), ()> {
		let xt = match Block::Extrinsic::decode(&mut &*data) {
			Ok(xt) => xt,
			Err(e) => {
				log::warn!("Unable to decode extrinsic: {:?}: {}", data, e.what());
				return Err(());
			},
		};

		self.pool.submit_at(&self.at, xt)
	}
}
