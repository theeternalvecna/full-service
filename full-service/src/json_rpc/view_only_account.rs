// Copyright (c) 2020-2021 MobileCoin Inc.

//! API definition for the View Only Account object.

use crate::db;
use serde_derive::{Deserialize, Serialize};
use std::convert::TryFrom;

/// An view-only-account in the wallet.
///
/// A view only account is associated with one private view key
#[derive(Deserialize, Serialize, Default, Debug, Clone)]
pub struct ViewOnlyAccount {
    /// String representing the object's type. Objects of the same type share
    /// the same value.
    pub object: String,

    /// Display name for the account.
    pub name: String,

    /// The private key used for viewing transactions for this account
    pub view_private_key: String,

    /// Index of the first block when this account may have received funds.
    /// No transactions before this point will be synchronized.
    pub first_block_index: String,

    /// Index of the next block this account needs to sync.
    pub next_block_index: String,
}

impl TryFrom<&db::models::ViewOnlyAccount> for ViewOnlyAccount {
    type Error = String;

    fn try_from(src: &db::models::ViewOnlyAccount) -> Result<ViewOnlyAccount, String> {
        Ok(ViewOnlyAccount {
            object: "view_only_account".to_string(),
            name: src.name.clone(),
            first_block_index: (src.first_block_index as u64).to_string(),
            next_block_index: (src.next_block_index as u64).to_string(),
            view_private_key: src.view_private_key.clone(),
        })
    }
}
