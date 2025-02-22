// Copyright 2017-2019 Parity Technologies (UK) Ltd.
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

//! Console informant. Prints sync progress and block events. Runs on the calling thread.

use client_api::BlockchainEvents;
use futures::{StreamExt, TryStreamExt, FutureExt, future, compat::Stream01CompatExt};
use log::{info, warn};
use sr_primitives::traits::Header;
use service::AbstractService;
use std::time::Duration;

mod display;

/// Creates an informant in the form of a `Future` that must be polled regularly.
pub fn build(service: &impl AbstractService) -> impl futures::Future<Output = ()> {
	let client = service.client();

	let mut display = display::InformantDisplay::new();

	let display_notifications = service
		.network_status(Duration::from_millis(5000))
		.compat()
		.try_for_each(move |(net_status, _)| {
			let info = client.info();
			display.display(&info, net_status);
			future::ok(())
		});

	let client = service.client();
	let mut last_best = {
		let info = client.info();
		Some((info.chain.best_number, info.chain.best_hash))
	};

	let display_block_import = client.import_notification_stream().for_each(move |n| {
		// detect and log reorganizations.
		if let Some((ref last_num, ref last_hash)) = last_best {
			if n.header.parent_hash() != last_hash && n.is_new_best  {
				let maybe_ancestor = header_metadata::lowest_common_ancestor(
					&*client,
					last_hash.clone(),
					n.hash,
				);

				match maybe_ancestor {
					Ok(ref ancestor) if ancestor.hash != *last_hash => info!(
						"Reorg from #{},{} to #{},{}, common ancestor #{},{}",
						last_num, last_hash,
						n.header.number(), n.hash,
						ancestor.number, ancestor.hash,
					),
					Ok(_) => {},
					Err(e) => warn!("Error computing tree route: {}", e),
				}
			}
		}

		if n.is_new_best {
			last_best = Some((n.header.number().clone(), n.hash.clone()));
		}

		info!(target: "substrate", "Imported #{} ({})", n.header.number(), n.hash);
		future::ready(())
	});

	future::join(
		display_notifications,
		display_block_import
	).map(|_| ())
}
