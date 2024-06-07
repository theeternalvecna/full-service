// Copyright (c) 2018-2020 MobileCoin Inc.

//! Manages ledger block scanning for wallet accounts.

use crate::{
    db::{
        account::{AccountID, AccountModel},
        assigned_subaddress::AssignedSubaddressModel,
        exclusive_transaction,
        models::{Account, AssignedSubaddress, TransactionLog, Txo},
        transaction_log::TransactionLogModel,
        txo::TxoModel,
        Conn, WalletDb,
    },
    error::SyncError,
};
use mc_account_keys::{AccountKey, ViewAccountKey};
use mc_common::{
    logger::{log, Logger},
    HashMap as MCHashMap,
};
use mc_crypto_keys::{RistrettoPrivate, RistrettoPublic};
use mc_ledger_db::{Ledger, LedgerDB};
use mc_transaction_core::{
    get_tx_out_shared_secret,
    onetime_keys::{recover_onetime_private_key, recover_public_subaddress_spend_key},
    ring_signature::KeyImage,
    tx::TxOut,
    Amount,
};
use rayon::prelude::*;

use reqwest::Url;
use reqwest::{
    blocking::Client, // FIXME: if we don't want this to be blocking, we'll be awaiting futures
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
};
use serde_json::json;
use std::{
    collections::HashMap,
    convert::TryFrom,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const BLOCKS_CHUNK_SIZE: u64 = 1_000;

/// Sync thread - holds objects needed to cleanly terminate the sync thread.
pub struct SyncThread {
    /// The main sync thread handle.
    join_handle: Option<thread::JoinHandle<()>>,

    /// Stop trigger, used to signal the thread to terminate.
    stop_requested: Arc<AtomicBool>,
}

// FIXME: refactor this to its own file - it's gotten big enough
pub struct WebhookThread {
    /// The main sync thread handle.
    join_handle: Option<thread::JoinHandle<()>>,

    /// Stop trigger, used to signal the thread to terminate.
    stop_requested: Arc<AtomicBool>,
}

impl WebhookThread {
    pub fn start(
        server_url: Url,
        accounts_with_deposits: Arc<Mutex<HashMap<AccountID, bool>>>,
        restart: Arc<Mutex<AtomicBool>>,
        logger: Logger,
    ) -> Self {
        // Start the webhook thread.

        let stop_requested = Arc::new(AtomicBool::new(false));
        let thread_stop_requested = stop_requested.clone();

        // Question: Should we consider only spawning a thread when there
        // have been received txos, and therefore something to send?
        // Answer: For now, we are ok with having this thread running all the time,
        // because it is lightweight enough. If we find that it is causing issues,
        // we will revisit. The solution would be to make this async, or to only
        // spawn the thread when there are txos to send. There may be an advantage to
        // leaving the connection open.

        let join_handle = Some(
            thread::Builder::new()
                .name("webhook".to_string())
                .spawn(move || {
                    log::debug!(logger, "Webhook thread started.");

                    let client = Client::builder()
                        .build()
                        .expect("Failed creating reqwest client");
                    let mut json_headers = HeaderMap::new();
                    json_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

                    loop {
                        if thread_stop_requested.load(Ordering::SeqCst) {
                            log::debug!(logger, "WebhookThread stop requested.");
                            break;
                        }
                        // Gather the current accounts_to_send, then wipe the contents
                        let accounts_to_send: Vec<_> = accounts_with_deposits
                            .lock()
                            .unwrap()
                            .clone()
                            .iter()
                            .filter(|&(_k, &v)| v == true)
                            .map(|(k, _v)| k.clone())
                            .collect();

                        // Delete the keys that we're alerting on
                        for key in accounts_to_send.iter() {
                            log::debug!(logger, "Account to send: {:?}", key);
                            accounts_with_deposits.lock().unwrap().remove(&key);
                        }

                        // Only read restart, do not set
                        let restart_to_send = restart.lock().unwrap().load(Ordering::Relaxed);

                        if accounts_to_send.len() > 0 {
                            log::debug!(logger, "@@@@ ABOUT TO SEND THE WEBHOOK");
                            // Question: will this keep the connection open? Or will it
                            // close the connection after this request?
                            match client
                                .post(server_url.clone())
                                .body(
                                    json!(
                                        {
                                            "accounts": accounts_to_send,
                                            "restart": restart_to_send,
                                        }
                                    )
                                    .to_string(),
                                )
                                .send()
                            {
                                Ok(response) => match response.error_for_status() {
                                    Ok(_) => (),
                                    Err(e) => {
                                        log::error!(
                                            logger,
                                            "Failed getting webhook response: {:?}",
                                            e
                                        );
                                    }
                                },
                                Err(e) => {
                                    log::error!(logger, "Failed sending webhook request: {:?}", e);
                                }
                            }
                        }
                        // FIXME: this duration should be as often as we are polling
                        // for new blocks from consensus
                        thread::sleep(Duration::from_millis(10));
                    }
                })
                .expect("failed starting webhook thread"),
        );
        Self {
            join_handle,
            stop_requested,
        }
    }
    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(join_handle) = self.join_handle.take() {
            join_handle.join().expect("WebhookThread join failed");
        }
    }
}

impl Drop for WebhookThread {
    fn drop(&mut self) {
        self.stop();
    }
}

impl SyncThread {
    pub fn start(
        ledger_db: LedgerDB,
        wallet_db: WalletDb,
        accounts_with_deposits: Arc<Mutex<HashMap<AccountID, bool>>>,
        _restart: Arc<Mutex<AtomicBool>>, // FIXME: when does this get set and by what?
        logger: Logger,
    ) -> Self {
        // Start the sync thread.

        let stop_requested = Arc::new(AtomicBool::new(false));
        let thread_stop_requested = stop_requested.clone();
        let thread_accounts_with_deposits = accounts_with_deposits.clone();

        let join_handle = Some(
            thread::Builder::new()
                .name("sync".to_string())
                .spawn(move || {
                    log::debug!(logger, "Sync thread started.");

                    let conn = &mut wallet_db
                        .get_pooled_conn()
                        .expect("failed getting wallet db connection");

                    loop {
                        if thread_stop_requested.load(Ordering::SeqCst) {
                            log::debug!(logger, "SyncThread stop requested.");
                            break;
                        }
                        // FIXME: is this a problem that we're cloning per loop?
                        match sync_all_accounts(
                            &ledger_db,
                            conn,
                            thread_accounts_with_deposits.clone(),
                            &logger,
                        ) {
                            Ok(()) => (),
                            Err(e) => log::error!(&logger, "Error during account sync:\n{:?}", e),
                        }
                        // This sleep is to allow other API calls that need access to the database a
                        // chance to execute, because the sync process requires a write lock on the
                        // database.
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    log::debug!(logger, "SyncThread stopped.");
                })
                .expect("failed starting main sync thread"),
        );

        Self {
            join_handle,
            stop_requested,
        }
    }

    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(join_handle) = self.join_handle.take() {
            join_handle.join().expect("SyncThread join failed");
        }
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn sync_all_accounts(
    ledger_db: &LedgerDB,
    conn: Conn,
    accounts_with_deposits: Arc<Mutex<HashMap<AccountID, bool>>>,
    logger: &Logger,
) -> Result<(), SyncError> {
    // Get the current number of blocks in ledger.
    let num_blocks = ledger_db
        .num_blocks()
        .expect("failed getting number of blocks");
    if num_blocks == 0 {
        return Ok(()); // FIXME: we want it to fire in this case with empty
                       // accounts
    }

    // Go over our list of accounts and see which ones need to process more blocks.
    let accounts: Vec<Account> =
        { Account::list_all(conn, None, None).expect("Failed getting accounts from database") };

    for account in accounts {
        // If there are no new blocks for this account, don't do anything.
        //
        // If the account is currently resyncing, we need to set it to false
        // here.
        if account.next_block_index as u64 > num_blocks - 1 {
            log::debug!(logger, "@@@ WE MADE IT TO THE END WE ARE FULLY SYNCED and accounts with new deposits = {:?}", accounts_with_deposits);

            // set list of accounts of deposits to be null
            // if it comes back with more than one, add the account to the list
            // then give the list of accounts in the webhook
            log::debug!(
                logger,
                "Setting accounts with deposits to {:?}",
                accounts_with_deposits
            );
            let mut account_set = accounts_with_deposits.lock().unwrap();
            account_set
                .entry(AccountID(account.id.clone()))
                .and_modify(|v| *v = true);
            // Note, we do not clear the accounts_with_deposit variable
            // here, because we want to ensure the webhook
            // fires with the right data, and only the
            // webhook thread can clear it.

            if account.resyncing {
                account.update_resyncing(false, conn)?;
            }

            continue;
        }
        // FIXME: we don't want to fire this off during resyncing - you could just fire
        // when caught up [when getting to the end fo the blockchain, fire, but not if
        // not caught up]
        let found_txos = sync_account_next_chunk(ledger_db, conn, &account.id, logger)?;
        if found_txos > 0 {
            log::debug!(logger, "Found some txos!");
            accounts_with_deposits
                .lock()
                .unwrap()
                .insert(AccountID(account.id), false);
            log::debug!(
                logger,
                "\t \t NOW ACCOUNTS WITH NEW DEPOSITS = {:?}",
                accounts_with_deposits
            );
        }
    }

    Ok(())
}

pub fn sync_account_next_chunk(
    ledger_db: &LedgerDB,
    conn: Conn,
    account_id_hex: &str,
    logger: &Logger,
) -> Result<usize, SyncError> {
    exclusive_transaction(conn, |conn| {
        // Get the account data. If it is no longer available, the account has been
        // removed and we can simply return.
        let account = Account::get(&AccountID(account_id_hex.to_string()), conn)?;

        let start_time = Instant::now();
        let start_block_index = account.next_block_index as u64;
        let mut end_block_index: Option<u64> = None;

        // Load transaction outputs and key images for this chunk.
        let mut tx_outs: Vec<(u64, TxOut)> = Vec::new();
        let mut key_images: Vec<(u64, KeyImage)> = Vec::new();

        let start = account.next_block_index as u64;
        let end = start + BLOCKS_CHUNK_SIZE;
        for block_index in start..end {
            let block_contents = match ledger_db.get_block_contents(block_index) {
                Ok(block_contents) => block_contents,
                Err(mc_ledger_db::Error::NotFound) => {
                    break;
                }
                Err(err) => {
                    return Err(err.into());
                }
            };
            end_block_index = Some(block_index);

            for tx_out in block_contents.outputs {
                tx_outs.push((block_index, tx_out));
            }

            for key_image in block_contents.key_images {
                key_images.push((block_index, key_image));
            }
        }

        // If no blocks were found, exit.
        if end_block_index.is_none() {
            return Ok(0);
        }
        let end_block_index = end_block_index.unwrap();

        let num_received_txos = if account.view_only {
            let view_account_key: ViewAccountKey = mc_util_serial::decode(&account.account_key)?;

            // Attempt to decode each transaction as received by this account.
            let received_txos: Vec<_> = tx_outs
                .into_par_iter()
                .filter_map(|(block_index, tx_out)| {
                    let amount = decode_amount(&tx_out, view_account_key.view_private_key())?;
                    Some((block_index, tx_out, amount))
                })
                .collect();

            let mut received_txos_with_subaddresses = Vec::new();
            for (block_index, tx_out, amount) in received_txos {
                let subaddress_index =
                    decode_subaddress_index(&tx_out, view_account_key.view_private_key(), conn);

                received_txos_with_subaddresses.push((
                    block_index,
                    tx_out,
                    amount,
                    subaddress_index,
                ));
            }

            let num_received_txos = received_txos_with_subaddresses.len();

            // Write received transactions to the database.
            for (block_index, tx_out, amount, subaddress_index) in received_txos_with_subaddresses {
                Txo::create_received(
                    tx_out.clone(),
                    subaddress_index,
                    None,
                    amount,
                    block_index,
                    account_id_hex,
                    conn,
                )?;
            }

            // Match key images to mark existing unspent transactions as spent.
            let unspent_key_images: MCHashMap<KeyImage, String> =
                Txo::list_unspent_or_pending_key_images(account_id_hex, None, conn)?;
            let spent_txos: Vec<(u64, String)> = key_images
                .into_par_iter()
                .filter_map(|(block_index, key_image)| {
                    unspent_key_images
                        .get(&key_image)
                        .map(|txo_id_hex| (block_index, txo_id_hex.clone()))
                })
                .collect();
            let num_spent_txos = spent_txos.len();

            for (block_index, txo_id_hex) in &spent_txos {
                Txo::update_spent_block_index(txo_id_hex, *block_index, conn)?;
                TransactionLog::update_pending_associated_with_txo_to_succeeded(
                    txo_id_hex,
                    *block_index,
                    conn,
                )?;
            }

            TransactionLog::update_pending_exceeding_tombstone_block_index_to_failed(
                end_block_index + 1,
                conn,
            )?;

            // Done syncing this chunk. Mark these blocks as synced for this account.
            account.update_next_block_index(end_block_index + 1, conn)?;

            let num_blocks_synced = end_block_index - start_block_index + 1;

            let duration = start_time.elapsed();

            log::debug!(
            logger,
            "Synced {} blocks ({}-{}) for account {} in {:?}. {} txos received, {}/{} txos spent.",
            num_blocks_synced,
            start_block_index,
            end_block_index,
            account_id_hex.chars().take(6).collect::<String>(),
            duration,
            num_received_txos,
            num_spent_txos,
            unspent_key_images.len()
        );
            num_received_txos
        } else {
            let account_key: AccountKey = mc_util_serial::decode(&account.account_key)?;

            // Attempt to decode each transaction as received by this account.
            let received_txos: Vec<_> = tx_outs
                .into_par_iter()
                .filter_map(|(block_index, tx_out)| {
                    let amount = decode_amount(&tx_out, account_key.view_private_key())?;
                    Some((block_index, tx_out, amount))
                })
                .collect();

            let mut received_txos_with_subaddresses_and_key_images = Vec::new();
            for (block_index, tx_out, amount) in received_txos {
                let (subaddress_index, key_image) =
                    decode_subaddress_and_key_image(&tx_out, &account_key, conn);

                received_txos_with_subaddresses_and_key_images.push((
                    block_index,
                    tx_out,
                    amount,
                    subaddress_index,
                    key_image,
                ));
            }

            let num_received_txos = received_txos_with_subaddresses_and_key_images.len();

            // Write received transactions to the database.
            for (block_index, tx_out, amount, subaddress_index, key_image) in
                received_txos_with_subaddresses_and_key_images
            {
                Txo::create_received(
                    tx_out.clone(),
                    subaddress_index,
                    key_image,
                    amount,
                    block_index,
                    account_id_hex,
                    conn,
                )?;
            }

            // Match key images to mark existing unspent transactions as spent.
            let unspent_key_images: MCHashMap<KeyImage, String> =
                Txo::list_unspent_or_pending_key_images(account_id_hex, None, conn)?;
            let spent_txos: Vec<(u64, String)> = key_images
                .into_par_iter()
                .filter_map(|(block_index, key_image)| {
                    unspent_key_images
                        .get(&key_image)
                        .map(|txo_id_hex| (block_index, txo_id_hex.clone()))
                })
                .collect();
            let num_spent_txos = spent_txos.len();
            for (block_index, txo_id_hex) in &spent_txos {
                Txo::update_spent_block_index(txo_id_hex, *block_index, conn)?;
                TransactionLog::update_pending_associated_with_txo_to_succeeded(
                    txo_id_hex,
                    *block_index,
                    conn,
                )?;
            }

            TransactionLog::update_pending_exceeding_tombstone_block_index_to_failed(
                end_block_index + 1,
                conn,
            )?;

            // Done syncing this chunk. Mark these blocks as synced for this account.
            account.update_next_block_index(end_block_index + 1, conn)?;

            let num_blocks_synced = end_block_index - start_block_index + 1;

            let duration = start_time.elapsed();

            log::debug!(
            logger,
            "Synced {} blocks ({}-{}) for account {} in {:?}. {} txos received, {}/{} txos spent.",
            num_blocks_synced,
            start_block_index,
            end_block_index,
            account_id_hex.chars().take(6).collect::<String>(),
            duration,
            num_received_txos,
            num_spent_txos,
            unspent_key_images.len()
        );
            num_received_txos
        };

        Ok(num_received_txos)
    })
}

/// Attempt to decode the transaction amount. If we can't, then this transaction
/// does not belong to this account.
pub fn decode_amount(tx_out: &TxOut, view_private_key: &RistrettoPrivate) -> Option<Amount> {
    let tx_public_key = match RistrettoPublic::try_from(&tx_out.public_key) {
        Err(_) => return None,
        Ok(k) => k,
    };
    let shared_secret = get_tx_out_shared_secret(view_private_key, &tx_public_key);
    match tx_out.get_masked_amount().ok()?.get_value(&shared_secret) {
        Ok((a, _)) => Some(a),
        Err(_) => None,
    }
}

pub fn decode_subaddress_index(
    tx_out: &TxOut,
    view_private_key: &RistrettoPrivate,
    conn: Conn,
) -> Option<u64> {
    let tx_public_key = match RistrettoPublic::try_from(&tx_out.public_key) {
        Ok(k) => k,
        Err(_) => return None,
    };
    let tx_out_target_key = match RistrettoPublic::try_from(&tx_out.target_key) {
        Ok(k) => k,
        Err(_) => return None,
    };

    let subaddress_spend_public_key: RistrettoPublic =
        recover_public_subaddress_spend_key(view_private_key, &tx_out_target_key, &tx_public_key);

    AssignedSubaddress::find_by_subaddress_spend_public_key(&subaddress_spend_public_key, conn)
        .ok()
        .map(|(idx, _b58)| idx as u64)
}

/// Attempt to match the target address with one of our subaddresses. This
/// should only be done on tx-outs that have already had their amounts decoded.
/// If this fails, then the transaction is "orphaned", meaning we haven't
/// generated the correct subaddress yet.
pub fn decode_subaddress_and_key_image(
    tx_out: &TxOut,
    account_key: &AccountKey,
    conn: Conn,
) -> (Option<u64>, Option<KeyImage>) {
    let tx_public_key = match RistrettoPublic::try_from(&tx_out.public_key) {
        Ok(k) => k,
        Err(_) => return (None, None),
    };

    let subaddress_index = decode_subaddress_index(tx_out, account_key.view_private_key(), conn);

    let key_image = if let Some(subaddress_i) = subaddress_index {
        let onetime_private_key = recover_onetime_private_key(
            &tx_public_key,
            account_key.view_private_key(),
            &account_key.subaddress_spend_private(subaddress_i),
        );
        Some(KeyImage::from(&onetime_private_key))
    } else {
        None
    };

    (subaddress_index, key_image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        service::{account::AccountService, balance::BalanceService, txo::TxoService},
        test_utils::{
            add_block_to_ledger_db, get_test_ledger, manually_sync_account, setup_wallet_service,
            MOB,
        },
    };
    use mc_account_keys::{AccountKey, RootEntropy, RootIdentity};
    use mc_common::logger::{test_with_logger, Logger};
    use mc_transaction_core::{tokens::Mob, Token};
    use mc_util_from_random::FromRandom;
    use rand::{rngs::StdRng, SeedableRng};

    #[test_with_logger]
    fn test_process_txo_bigint_in_origin(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let entropy = RootEntropy::from_random(&mut rng);
        let account_key = AccountKey::from(&RootIdentity::from(&entropy));

        let mut ledger_db = get_test_ledger(0, &[], 0, &mut rng);

        let origin_block_amount: u128 = 250_000_000 * MOB as u128;
        let origin_block_txo_amount = origin_block_amount / 16;
        let o = account_key.subaddress(0);
        let _new_block_index = add_block_to_ledger_db(
            &mut ledger_db,
            &[
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o.clone(),
                o,
            ],
            origin_block_txo_amount as u64,
            &[],
            &mut rng,
        );

        let service = setup_wallet_service(ledger_db.clone(), None, logger.clone());
        let wallet_db = &service.wallet_db.as_ref().unwrap();

        // Import the account
        let _account = service
            .import_account_from_legacy_root_entropy(
                hex::encode(entropy.bytes),
                None,
                None,
                None,
                "".to_string(),
                "".to_string(),
                false,
            )
            .expect("Could not import account entropy");

        manually_sync_account(
            &ledger_db,
            wallet_db,
            &AccountID::from(&account_key),
            &logger,
        );

        // There should now be 16 txos. Let's get each one and verify the amount
        let expected_value = 15_625_000 * MOB;

        let txo_infos = service
            .list_txos(
                Some(AccountID::from(&account_key).to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        for txo_info in txo_infos {
            assert_eq!(txo_info.txo.value as u64, expected_value);
        }

        // Now verify that the service gets the balance with the correct value
        let balance = service
            .get_balance_for_account(&AccountID::from(&account_key))
            .expect("Could not get balance");
        let balance_pmob = balance.get(&Mob::ID).unwrap();
        assert_eq!(balance_pmob.unspent, 250_000_000 * MOB as u128);
    }
}
