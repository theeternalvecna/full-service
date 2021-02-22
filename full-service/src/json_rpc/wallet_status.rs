// Copyright (c) 2020-2021 MobileCoin Inc.

//! API definition for the Wallet Status object.

use serde_derive::{Deserialize, Serialize};
use serde_json::Map;

/// The status of the wallet, including the sum of the balances for all
/// accounts.
#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct WalletStatus {
    /// String representing the object's type. Objects of the same type share
    /// the same value.
    pub object: String,

    /// The block count of MobileCoin's distributed ledger. The
    /// local_block_count is synced when it reaches the network_block_count.
    pub network_block_count: String,

    /// The local block count downloaded from the ledger. The local database
    /// will sync up to the network_block_count. The account_block_count can
    /// only sync up to local_block_count.
    pub local_block_count: String,

    /// Whether ALL accounts are synced with the network_block_count. Balances
    /// may not appear correct if any account is still syncing.
    pub is_synced_all: bool,

    /// Unspent pico mob for ALL accounts at the account_block_count. If the
    /// account is syncing, this value may change.
    pub total_unspent_pmob: String,

    /// Pending out-going pico mob from ALL accounts. Pending pico mobs will
    /// clear once the ledger processes the outoing txo. The available_pmob will
    /// reflect the change.
    pub total_pending_pmob: String,

    /// A list of all account_ids imported into the wallet in order of import.
    pub account_ids: Vec<String>,

    /// A normalized hash mapping account_id to account objects.
    pub account_map: Map<String, serde_json::Value>,
}
