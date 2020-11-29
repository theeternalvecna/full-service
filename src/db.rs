// Copyright (c) 2020 MobileCoin Inc.

//! Provides the CRUD implementations for our DB, and converts types to what is expected
//! by the DB.

use mc_account_keys::{AccountKey, PublicAddress, DEFAULT_SUBADDRESS_INDEX};
use mc_common::logger::{log, Logger};
use mc_crypto_digestible::{Digestible, MerlinTranscript};
use mc_crypto_keys::RistrettoPublic;
use mc_mobilecoind::payments::TxProposal;
use mc_transaction_core::ring_signature::KeyImage;
use mc_transaction_core::tx::TxOut;

use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::RunQueryDsl;

use crate::error::WalletDbError;
use crate::models::{
    Account, AccountTxoStatus, AssignedSubaddress, NewAccount, NewAccountTxoStatus,
    NewAssignedSubaddress, NewTxo, Txo,
};
// Schema Tables
use crate::schema::account_txo_statuses as schema_account_txo_statuses;
use crate::schema::accounts as schema_accounts;
use crate::schema::assigned_subaddresses as schema_assigned_subaddresses;
use crate::schema::txos as schema_txos;

// Query Objects
use crate::schema::account_txo_statuses::dsl::account_txo_statuses as dsl_account_txo_statuses;
use crate::schema::accounts::dsl::accounts as dsl_accounts;
use crate::schema::txos::dsl::txos as dsl_txos;

// Helper method to use our PrintableWrapper to b58 encode the PublicAddress
pub fn b58_encode(public_address: &PublicAddress) -> Result<String, WalletDbError> {
    let mut wrapper = mc_mobilecoind_api::printable::PrintableWrapper::new();
    wrapper.set_public_address(public_address.into());
    Ok(wrapper.b58_encode()?)
}

#[derive(Debug)]
pub struct AccountID(String);

impl From<&AccountKey> for AccountID {
    fn from(src: &AccountKey) -> AccountID {
        let main_subaddress = src.subaddress(DEFAULT_SUBADDRESS_INDEX);
        /// The account ID is derived from the contents of the account key
        #[derive(Digestible)]
        struct ConstAccountData {
            /// The public address of the main subaddress for this account
            pub address: PublicAddress,
        }
        let const_data = ConstAccountData {
            address: main_subaddress.clone(),
        };
        let temp: [u8; 32] = const_data.digest32::<MerlinTranscript>(b"account_data");
        Self(hex::encode(temp))
    }
}

impl AccountID {
    pub fn to_string(&self) -> String {
        self.0.clone()
    }
}

#[derive(Debug)]
pub struct TxoID(String);

impl From<&TxOut> for TxoID {
    fn from(src: &TxOut) -> TxoID {
        /// The txo ID is derived from the contents of the txo
        #[derive(Digestible)]
        struct ConstTxoData {
            /// The public address of the main subaddress for this account
            pub txo: TxOut,
        }
        let const_data = ConstTxoData { txo: src.clone() };
        let temp: [u8; 32] = const_data.digest32::<MerlinTranscript>(b"txo_data");
        Self(hex::encode(temp))
    }
}

impl TxoID {
    pub fn to_string(&self) -> String {
        self.0.clone()
    }
}

#[derive(Clone)]
pub struct WalletDb {
    pool: Pool<ConnectionManager<SqliteConnection>>,
    logger: Logger,
}

impl WalletDb {
    pub fn new(pool: Pool<ConnectionManager<SqliteConnection>>, logger: Logger) -> Self {
        Self { pool, logger }
    }

    pub fn new_from_url(database_url: &str, logger: Logger) -> Result<Self, WalletDbError> {
        let manager = ConnectionManager::<SqliteConnection>::new(database_url);
        let pool = Pool::builder()
            .max_size(1)
            .test_on_check_out(true)
            .build(manager)?;
        Ok(Self::new(pool, logger))
    }

    /// Create a new account.
    pub fn create_account(
        &self,
        account_key: &AccountKey,
        main_subaddress_index: u64,
        change_subaddress_index: u64,
        next_subaddress_index: u64,
        first_block: u64,
        next_block: u64,
        name: &str,
    ) -> Result<(String, String), WalletDbError> {
        let conn = self.pool.get()?;

        let main_subaddress = account_key.subaddress(main_subaddress_index);
        let account_id = AccountID::from(account_key);

        // FIXME: It's concerning to lose a bit of precision in casting to i64
        let new_account = NewAccount {
            account_id_hex: &account_id.0,
            encrypted_account_key: &mc_util_serial::encode(account_key), // FIXME: add encryption
            main_subaddress_index: main_subaddress_index as i64,
            change_subaddress_index: change_subaddress_index as i64,
            next_subaddress_index: next_subaddress_index as i64,
            first_block: first_block as i64,
            next_block: next_block as i64,
            name,
        };

        diesel::insert_into(schema_accounts::table)
            .values(&new_account)
            .execute(&conn)?;

        // Insert the assigned subaddresses for main and change
        let main_subaddress_b58 = b58_encode(&main_subaddress)?;
        let main_subaddress_entry = NewAssignedSubaddress {
            assigned_subaddress_b58: &main_subaddress_b58,
            account_id_hex: &account_id.0,
            address_book_entry: None, // FIXME: Address Book Entry if details provided, or None always for main?
            public_address: &mc_util_serial::encode(&main_subaddress),
            subaddress_index: main_subaddress_index as i64,
            comment: "Main",
            expected_value: None,
            subaddress_spend_key: &mc_util_serial::encode(main_subaddress.spend_public_key()),
        };

        diesel::insert_into(schema_assigned_subaddresses::table)
            .values(&main_subaddress_entry)
            .execute(&conn)?;

        let change_subaddress = account_key.subaddress(change_subaddress_index);
        let change_subaddress_b58 = b58_encode(&change_subaddress)?;
        let change_subaddress_entry = NewAssignedSubaddress {
            assigned_subaddress_b58: &change_subaddress_b58,
            account_id_hex: &account_id.0,
            address_book_entry: None, // FIXME: Address Book Entry if details provided, or None always for main?
            public_address: &mc_util_serial::encode(&change_subaddress),
            subaddress_index: change_subaddress_index as i64,
            comment: "Change",
            expected_value: None,
            subaddress_spend_key: &mc_util_serial::encode(change_subaddress.spend_public_key()),
        };

        diesel::insert_into(schema_assigned_subaddresses::table)
            .values(&change_subaddress_entry)
            .execute(&conn)?;

        Ok((account_id.0, main_subaddress_b58))
    }

    /// List all accounts.
    pub fn list_accounts(&self) -> Result<Vec<Account>, WalletDbError> {
        let conn = self.pool.get()?;

        let results: Vec<Account> = schema_accounts::table
            .select(schema_accounts::all_columns)
            .load::<Account>(&conn)?;
        Ok(results)
    }

    /// Get a specific account
    pub fn get_account(&self, account_id_hex: &str) -> Result<Account, WalletDbError> {
        let conn = self.pool.get()?;

        match dsl_accounts
            .find(account_id_hex)
            .get_result::<Account>(&conn)
        {
            Ok(a) => Ok(a),
            // Match on NotFound to get a more informative NotFound Error
            Err(diesel::result::Error::NotFound) => {
                Err(WalletDbError::NotFound(account_id_hex.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Update an account.
    /// The only updatable field is the name. Any other desired update requires adding
    /// a new account, and deleting the existing if desired.
    pub fn update_account_name(
        &self,
        account_id_hex: &str,
        new_name: String,
    ) -> Result<(), WalletDbError> {
        let conn = self.pool.get()?;

        diesel::update(dsl_accounts.find(account_id_hex))
            .set(schema_accounts::name.eq(new_name))
            .execute(&conn)?;
        Ok(())
    }

    /// Delete an account.
    pub fn delete_account(&self, account_id_hex: &str) -> Result<(), WalletDbError> {
        let conn = self.pool.get()?;

        diesel::delete(dsl_accounts.find(account_id_hex)).execute(&conn)?;
        Ok(())
    }

    /// Create a TXO entry
    pub fn create_received_txo(
        &self,
        txo: TxOut,
        subaddress_index: u64,
        key_image: KeyImage,
        value: u64,
        received_block_height: i64,
        account_id_hex: &str,
    ) -> Result<String, WalletDbError> {
        let conn = self.pool.get()?;

        let txo_id = TxoID::from(&txo);

        let key_image_bytes = mc_util_serial::encode(&key_image);
        let new_txo = NewTxo {
            txo_id_hex: &txo_id.0,
            value: value as i64,
            target_key: &mc_util_serial::encode(&txo.target_key),
            public_key: &mc_util_serial::encode(&txo.public_key),
            e_fog_hint: &mc_util_serial::encode(&txo.e_fog_hint),
            txo: &mc_util_serial::encode(&txo),
            subaddress_index: subaddress_index as i64,
            key_image: Some(&key_image_bytes),
            received_block_height: Some(received_block_height as i64),
            spent_tombstone_block_height: None,
            spent_block_height: None,
            proof: None,
        };

        diesel::insert_into(schema_txos::table)
            .values(&new_txo)
            .execute(&conn)?;

        let new_account_txo_status = NewAccountTxoStatus {
            account_id_hex: &account_id_hex,
            txo_id_hex: &txo_id.0,
            txo_status: "unspent",
            txo_type: "received",
        };

        diesel::insert_into(schema_account_txo_statuses::table)
            .values(&new_account_txo_status)
            .execute(&conn)?;

        Ok(txo_id.0)
    }

    /// List all txos for a given account.
    pub fn list_txos(
        &self,
        account_id_hex: &str,
    ) -> Result<Vec<(Txo, AccountTxoStatus)>, WalletDbError> {
        let conn = self.pool.get()?;

        let results: Vec<(Txo, AccountTxoStatus)> = schema_txos::table
            .inner_join(
                schema_account_txo_statuses::table.on(schema_txos::txo_id_hex
                    .eq(schema_account_txo_statuses::txo_id_hex)
                    .and(schema_account_txo_statuses::account_id_hex.eq(account_id_hex))),
            )
            .select((
                schema_txos::all_columns,
                schema_account_txo_statuses::all_columns,
            ))
            .load(&conn)?;
        Ok(results)
    }

    pub fn list_unspent_txos(&self, account_id_hex: &str) -> Result<Vec<Txo>, WalletDbError> {
        let conn = self.pool.get()?;

        let results: Vec<Txo> = schema_txos::table
            .inner_join(
                schema_account_txo_statuses::table.on(schema_txos::txo_id_hex
                    .eq(schema_account_txo_statuses::txo_id_hex)
                    .and(schema_account_txo_statuses::account_id_hex.eq(account_id_hex))
                    .and(schema_account_txo_statuses::txo_status.eq("unspent"))),
            )
            .select(schema_txos::all_columns)
            .load(&conn)?;
        Ok(results)
    }

    pub fn select_txos_by_id(
        &self,
        account_id_hex: &str,
        txo_ids: &Vec<String>,
    ) -> Result<Vec<(Txo, AccountTxoStatus)>, WalletDbError> {
        let conn = self.pool.get()?;

        let mut results: Vec<(Txo, AccountTxoStatus)> = Vec::new();
        for txo_id in txo_ids {
            match dsl_txos.find(txo_id).get_result::<Txo>(&conn) {
                Ok(txo) => {
                    // Check that this txo is indeed owned by the account we think it is
                    match dsl_account_txo_statuses
                        .find((account_id_hex, txo_id))
                        .get_result::<AccountTxoStatus>(&conn)
                    {
                        Ok(status) => {
                            results.push((txo, status));
                        }
                        Err(diesel::result::Error::NotFound) => {
                            return Err(WalletDbError::NotFound(format!(
                                "Txo({:?}) found, but does not belong to Account({:?})",
                                txo_id, account_id_hex
                            )));
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
                Err(diesel::result::Error::NotFound) => {
                    return Err(WalletDbError::NotFound(txo_id.to_string()));
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        Ok(results)
    }

    pub fn select_unspent_txos_for_value(
        &self,
        account_id_hex: &str,
        max_spendable_value: i64,
    ) -> Result<Vec<Txo>, WalletDbError> {
        let conn = self.pool.get()?;

        let results: Vec<Txo> = schema_txos::table
            .inner_join(
                schema_account_txo_statuses::table.on(schema_txos::txo_id_hex
                    .eq(schema_account_txo_statuses::txo_id_hex)
                    .and(schema_account_txo_statuses::account_id_hex.eq(account_id_hex))
                    .and(schema_account_txo_statuses::txo_status.eq("unspent"))
                    .and(schema_txos::value.lt(max_spendable_value))),
            )
            .select(schema_txos::all_columns)
            .order_by(schema_txos::value.desc())
            .load(&conn)?;

        Ok(results)
    }

    /// List all subaddresses for a given account.
    pub fn list_subaddresses(
        &self,
        account_id_hex: &str,
    ) -> Result<Vec<AssignedSubaddress>, WalletDbError> {
        let conn = self.pool.get()?;

        let results: Vec<AssignedSubaddress> = schema_accounts::table
            .inner_join(
                schema_assigned_subaddresses::table.on(schema_accounts::account_id_hex
                    .eq(schema_assigned_subaddresses::account_id_hex)
                    .and(schema_accounts::account_id_hex.eq(account_id_hex))),
            )
            .select(schema_assigned_subaddresses::all_columns)
            .load(&conn)?;

        Ok(results)
    }

    pub fn get_subaddress_index_by_subaddress_spend_public_key(
        &self,
        subaddress_spend_public_key: &RistrettoPublic,
    ) -> Result<(i64, String), WalletDbError> {
        let conn = self.pool.get()?;

        let matches = schema_assigned_subaddresses::table
            .select((
                schema_assigned_subaddresses::subaddress_index,
                schema_assigned_subaddresses::account_id_hex,
            ))
            .filter(
                schema_assigned_subaddresses::subaddress_spend_key
                    .eq(mc_util_serial::encode(subaddress_spend_public_key)),
            )
            .load::<(i64, String)>(&conn)?;

        if matches.len() == 0 {
            Err(WalletDbError::NotFound(format!(
                "{:?}",
                subaddress_spend_public_key
            )))
        } else if matches.len() > 1 {
            Err(WalletDbError::DuplicateEntries(format!(
                "{:?}",
                subaddress_spend_public_key
            )))
        } else {
            Ok(matches[0].clone())
        }
    }

    pub fn update_spent_and_increment_next_block(
        &self,
        account_id_hex: &str,
        spent_block_height: i64,
        key_images: Vec<KeyImage>,
    ) -> Result<(), WalletDbError> {
        let conn = self.pool.get()?;

        for key_image in key_images {
            // Get the txo by key_image
            let matches = schema_txos::table
                .select(schema_txos::all_columns)
                .filter(schema_txos::key_image.eq(mc_util_serial::encode(&key_image)))
                .load::<Txo>(&conn)?;

            if matches.len() == 0 {
                // Not Found is ok - this means it's a key_image not associated with any of our txos
                continue;
            } else if matches.len() > 1 {
                return Err(WalletDbError::DuplicateEntries(format!(
                    "Key Image: {:?}",
                    key_image
                )));
            } else {
                // Update the TXO
                log::trace!(
                    self.logger,
                    "Updating spent for account {:?} at block height {:?} with key_image {:?}",
                    account_id_hex,
                    spent_block_height,
                    key_image
                );
                diesel::update(dsl_txos.find(&matches[0].txo_id_hex))
                    .set(schema_txos::spent_block_height.eq(Some(spent_block_height)))
                    .execute(&conn)?;

                // Update the AccountTxoStatus
                diesel::update(
                    dsl_account_txo_statuses.find((account_id_hex, &matches[0].txo_id_hex)),
                )
                .set(schema_account_txo_statuses::txo_status.eq("spent".to_string()))
                .execute(&conn)?;

                // FIXME: make sure the path for all txo_statuses and txo_types exist and are tested
            }
        }
        diesel::update(dsl_accounts.find(account_id_hex))
            .set(schema_accounts::next_block.eq(spent_block_height + 1))
            .execute(&conn)?;
        Ok(())
    }

    pub fn update_submitted_transaction(
        &self,
        tx_proposal: TxProposal,
    ) -> Result<(), WalletDbError> {
        let conn = self.pool.get()?;

        // FIXME: make these updates atomic

        // First update all inputs to "pending." They will remain pending until their key_image
        // hits the ledger.
        let account_id_hex = {
            let mut account_id = None;
            for utxo in tx_proposal.utxos.iter() {
                // Get the associated TxoID
                let txo_id = TxoID::from(&utxo.tx_out);

                // Find the account associated with this Txo
                let matches = schema_account_txo_statuses::table
                    .select(schema_account_txo_statuses::account_id_hex)
                    .filter(schema_account_txo_statuses::txo_id_hex.eq(txo_id.0.clone()))
                    .load::<String>(&conn)?;

                if matches.is_empty() {
                    return Err(WalletDbError::NotFound(txo_id.0.clone()));
                } else if matches.len() > 1 {
                    return Err(WalletDbError::DuplicateEntries(txo_id.0.clone()));
                } else {
                    let account_id_hex = matches[0].clone();
                    account_id = Some(account_id_hex.clone());

                    // Update the status
                    diesel::update(
                        dsl_account_txo_statuses.find((account_id_hex.clone(), &txo_id.0)),
                    )
                    .set(schema_account_txo_statuses::txo_status.eq("pending".to_string()))
                    .execute(&conn)?;
                }
            }
            if let Some(account_id_hex) = account_id {
                account_id_hex
            } else {
                return Err(WalletDbError::MultipleAccountIDsInTransaction);
            }
        };

        // Next, add all of our minted outputs to the Txo Table
        for (i, output) in tx_proposal.tx.prefix.outputs.iter().enumerate() {
            let txo_id = TxoID::from(output);

            // FIXME: currently only have the value and proofs for outlays, not change - will need
            //        to amend what's saved in the TxProposal to include change as outlays
            let (value, proof) = if let Some(outlay_index) = tx_proposal
                .outlay_index_to_tx_out_index
                .iter()
                .find_map(|(k, &v)| if v == i { Some(k) } else { None })
            {
                (
                    tx_proposal.outlays[outlay_index.clone()].value,
                    Some(outlay_index.clone()),
                )
            } else {
                (0, None)
            };

            // FIXME: Note, the subaddress_index is missing from the minted txo - do we want
            //        this to be the subaddress from which it was minted? We only have that during
            //        construction.
            let subaddress_index = 0;

            let encoded_proof =
                proof.map(|p| mc_util_serial::encode(&tx_proposal.outlay_confirmation_numbers[p]));

            let new_txo = NewTxo {
                txo_id_hex: &txo_id.0,
                value: value as i64,
                target_key: &mc_util_serial::encode(&output.target_key),
                public_key: &mc_util_serial::encode(&output.public_key),
                e_fog_hint: &mc_util_serial::encode(&output.e_fog_hint),
                txo: &mc_util_serial::encode(output),
                subaddress_index: subaddress_index as i64,
                key_image: None, // Only the recipient can calculate the KeyImage
                received_block_height: None,
                spent_tombstone_block_height: Some(tx_proposal.tx.prefix.tombstone_block as i64),
                spent_block_height: None,
                proof: encoded_proof.as_ref(),
            };

            diesel::insert_into(schema_txos::table)
                .values(&new_txo)
                .execute(&conn)?;

            let new_account_txo_status = NewAccountTxoStatus {
                account_id_hex: &account_id_hex,
                txo_id_hex: &txo_id.0,
                txo_status: "unknown", // We cannot track spent status for minted TXOs unless change
                txo_type: "minted",
            };

            diesel::insert_into(schema_account_txo_statuses::table)
                .values(&new_account_txo_status)
                .execute(&conn)?;
        }

        // FIXME: TODO: create a transaction table entry

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::WalletDbTestContext;
    use mc_account_keys::RootIdentity;
    use mc_common::logger::{test_with_logger, Logger};
    use mc_crypto_keys::{RistrettoPrivate, RistrettoPublic};
    use mc_transaction_core::encrypted_fog_hint::EncryptedFogHint;
    use mc_transaction_core::onetime_keys::recover_public_subaddress_spend_key;
    use mc_transaction_core::ring_signature::KeyImage;
    use mc_util_from_random::FromRandom;
    use rand::{rngs::StdRng, SeedableRng};
    use std::collections::HashSet;
    use std::convert::TryFrom;
    use std::iter::FromIterator;

    #[test_with_logger]
    fn test_account_crud(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let db_test_context = WalletDbTestContext::default();
        let walletdb = db_test_context.get_db_instance(logger);

        let account_key = AccountKey::random(&mut rng);
        let (account_id_hex, _public_address_b58) = walletdb
            .create_account(&account_key, 0, 1, 2, 0, 1, "Alice's Main Account")
            .unwrap();

        let res = walletdb.list_accounts().unwrap();
        assert_eq!(res.len(), 1);

        let acc = walletdb.get_account(&account_id_hex).unwrap();
        let expected_account = Account {
            account_id_hex: account_id_hex.clone(),
            encrypted_account_key: mc_util_serial::encode(&account_key),
            main_subaddress_index: 0,
            change_subaddress_index: 1,
            next_subaddress_index: 2,
            first_block: 0,
            next_block: 1,
            name: "Alice's Main Account".to_string(),
        };
        assert_eq!(expected_account, acc);

        // Verify that the subaddress table entries were updated for main and change
        let subaddresses = walletdb.list_subaddresses(&account_id_hex).unwrap();
        assert_eq!(subaddresses.len(), 2);
        let subaddress_indices: HashSet<i64> =
            HashSet::from_iter(subaddresses.iter().map(|s| s.subaddress_index));
        assert!(subaddress_indices.get(&0).is_some());
        assert!(subaddress_indices.get(&1).is_some());

        // Verify that we can get the correct subaddress index from the spend public key
        let main_subaddress = account_key.subaddress(0);
        let (retrieved_index, retrieved_acocunt_id_hex) = walletdb
            .get_subaddress_index_by_subaddress_spend_public_key(main_subaddress.spend_public_key())
            .unwrap();
        assert_eq!(retrieved_index, 0);
        assert_eq!(retrieved_acocunt_id_hex, account_id_hex);

        // Add another account with no name, scanning from later
        let account_key_secondary = AccountKey::from(&RootIdentity::from_random(&mut rng));
        let (account_id_hex_secondary, _public_address_b58_secondary) = walletdb
            .create_account(&account_key_secondary, 0, 1, 2, 50, 51, "")
            .unwrap();
        let res = walletdb.list_accounts().unwrap();
        assert_eq!(res.len(), 2);

        let acc_secondary = walletdb.get_account(&account_id_hex_secondary).unwrap();
        let mut expected_account_secondary = Account {
            account_id_hex: account_id_hex_secondary.clone(),
            encrypted_account_key: mc_util_serial::encode(&account_key_secondary),
            main_subaddress_index: 0,
            change_subaddress_index: 1,
            next_subaddress_index: 2,
            first_block: 50,
            next_block: 51,
            name: "".to_string(),
        };
        assert_eq!(expected_account_secondary, acc_secondary);

        // Update the name for the secondary account
        walletdb
            .update_account_name(
                &account_id_hex_secondary,
                "Alice's Secondary Account".to_string(),
            )
            .unwrap();
        let acc_secondary2 = walletdb.get_account(&account_id_hex_secondary).unwrap();
        expected_account_secondary.name = "Alice's Secondary Account".to_string();
        assert_eq!(expected_account_secondary, acc_secondary2);

        // Delete the secondary account
        walletdb.delete_account(&account_id_hex_secondary).unwrap();

        let res = walletdb.list_accounts().unwrap();
        assert_eq!(res.len(), 1);

        // Attempt to get the deleted account
        let res = walletdb.get_account(&account_id_hex_secondary);
        match res {
            Ok(_) => panic!("Should have deleted account"),
            Err(WalletDbError::NotFound(s)) => assert_eq!(s, account_id_hex_secondary.to_string()),
            Err(_) => panic!("Should error with NotFound but got {:?}", res),
        }
    }

    #[test_with_logger]
    fn test_received_tx_lifecycle(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let db_test_context = WalletDbTestContext::default();
        let walletdb = db_test_context.get_db_instance(logger);

        let account_key = AccountKey::random(&mut rng);
        let (account_id_hex, _public_address_b58) = walletdb
            .create_account(&account_key, 0, 1, 2, 0, 1, "Alice's Main Account")
            .unwrap();

        // FIXME: get recipient via the assigned subaddresses table, not directly
        let recipient = account_key.subaddress(0);

        // Create TXO for the account
        let tx_private_key = RistrettoPrivate::from_random(&mut rng);
        let hint = EncryptedFogHint::fake_onetime_hint(&mut rng);
        let value = 10;
        let txo = TxOut::new(value, &recipient, &tx_private_key, hint).unwrap();

        // Get KeyImage from the onetime private key
        let key_image = KeyImage::from(&tx_private_key);

        // Sanity check: Ensure that we can recover the subaddress
        // FIXME: Assert that the public address and the subaddress spend key was added to the
        //        assigned_subaddresses table
        let _subaddress_index = recover_public_subaddress_spend_key(
            account_key.view_private_key(),
            &RistrettoPublic::try_from(&txo.target_key).unwrap(),
            &RistrettoPublic::try_from(&txo.public_key).unwrap(),
        );
        let subaddress_index = 0;

        let received_block_height = 144;

        let txo_hex = walletdb
            .create_received_txo(
                txo.clone(),
                subaddress_index,
                key_image,
                value,
                received_block_height,
                &account_id_hex,
            )
            .unwrap();

        let txos = walletdb.list_txos(&account_id_hex).unwrap();
        assert_eq!(txos.len(), 1);

        let expected_txo = Txo {
            txo_id_hex: txo_hex.clone(),
            value: value as i64,
            target_key: mc_util_serial::encode(&txo.target_key),
            public_key: mc_util_serial::encode(&txo.public_key),
            e_fog_hint: mc_util_serial::encode(&txo.e_fog_hint),
            txo: mc_util_serial::encode(&txo),
            subaddress_index: subaddress_index as i64,
            key_image: Some(mc_util_serial::encode(&key_image)),
            received_block_height: Some(received_block_height as i64),
            spent_tombstone_block_height: None,
            spent_block_height: None,
            proof: None,
        };
        // Verify that the statuses table was updated correctly
        let expected_txo_status = AccountTxoStatus {
            account_id_hex: account_id_hex.clone(),
            txo_id_hex: txo_hex,
            txo_status: "unspent".to_string(),
            txo_type: "received".to_string(),
        };
        assert_eq!(txos[0].0, expected_txo);
        assert_eq!(txos[0].1, expected_txo_status);

        // Verify that the unspent filter works as well
        let unspent = walletdb.list_unspent_txos(&account_id_hex).unwrap();
        assert_eq!(unspent.len(), 1);

        // Now we'll "spend" the TXO
        // FIXME TODO: construct transaction proposal to spend it, maybe needs a helper in test_utils
        // self.update_submitted_transaction(tx_proposal)?;

        // Now we'll process the ledger and verify that the TXO was spent
        let spent_block_height = 365;

        walletdb
            .update_spent_and_increment_next_block(
                &account_id_hex,
                spent_block_height,
                vec![key_image],
            )
            .unwrap();

        let txos = walletdb.list_txos(&account_id_hex).unwrap();
        assert_eq!(txos.len(), 1);
        assert_eq!(
            txos[0].0.spent_block_height.unwrap(),
            spent_block_height as i64
        );
        assert_eq!(txos[0].1.txo_status, "spent".to_string());

        // Verify that the next block height is + 1
        let account = walletdb.get_account(&account_id_hex).unwrap();
        assert_eq!(account.next_block, spent_block_height + 1);

        // Verify that there are no unspent txos
        let unspent = walletdb.list_unspent_txos(&account_id_hex).unwrap();
        assert!(unspent.is_empty());
    }
}
