// Copyright (c) 2020-2021 MobileCoin Inc.

//! Service for managing gift codes.
//!
//! Gift codes are onetime accounts that contain a single Txo. They provide
//! a means to send MOB in a way that can be "claimed," for example, by pasting
//! a QR code for a gift code into a group chat, and the first person to
//! consume the gift code claims the MOB.

use crate::{
    db::{
        account::{AccountID, AccountModel},
        b58_encode,
        gift_code::{GiftCodeModel},
        models::{Account, GiftCode, TransactionLog},
        WalletDbError,
    },
    service::{
        account::{AccountServiceError},
        address::AddressService,
        transaction::{TransactionService, TransactionServiceError},
        WalletService,
    },
};
use displaydoc::Display;
use mc_account_keys::{AccountKey, RootEntropy, RootIdentity, DEFAULT_SUBADDRESS_INDEX};
use mc_common::logger::log;
use mc_connection::{BlockchainConnection, UserTxConnection};
use mc_crypto_keys::{CompressedRistrettoPublic, RistrettoPublic};
use mc_fog_report_validation::FogPubkeyResolver;
use mc_ledger_db::Ledger;
use mc_mobilecoind::payments::TxProposal;
use mc_transaction_core::{
    constants::MINIMUM_FEE, get_tx_out_shared_secret, onetime_keys::recover_onetime_private_key,
    ring_signature::KeyImage,
};
use mc_util_from_random::FromRandom;
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, fmt};

#[derive(Display, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum GiftCodeServiceError {
    /// Error interacting with the database: {0}
    Database(WalletDbError),

    /// Error with LedgerDB: {0}
    LedgerDB(mc_ledger_db::Error),

    /// Error decoding from hex: {0}
    HexDecode(hex::FromHexError),

    /// Error decoding prost: {0}
    ProstDecode(prost::DecodeError),

    /// Building the gift code failed
    BuildGiftCodeFailed,

    /// Unexpected TxStatus while polling: {0}
    UnexpectedTxStatus(String),

    /// Gift Code transaction produced an unexpected number of outputs: {0}
    UnexpectedNumOutputs(usize),

    /// Gift Code does not contain enough value to cover the fee: {0}
    InsufficientValueForFee(u64),

    /// Unexpected number of Txos in the Gift Code Account: {0}
    UnexpectedNumTxosInGiftCodeAccount(usize),

    /// Unexpected Value in Gift Code Txo: {0}
    UnexpectedValueInGiftCodeTxo(u64),

    /// The Txo is not consumable
    TxoNotConsumable,

    /// The TxProposal for this GiftCode was constructed in an unexpected
    /// manner.
    UnexpectedTxProposalFormat,

    /// Diesel error: {0}
    Diesel(diesel::result::Error),

    /// Error with the Transaction Service: {0}
    TransactionService(TransactionServiceError),

    /// Error with the Account Service: {0}
    AccountService(AccountServiceError),

    /// Error with printable wrapper: {0}
    PrintableWrapper(mc_api::display::Error),

    /// Error with crypto keys: {0}
    CryptoKey(mc_crypto_keys::KeyError),

    /// Gift Code Txo is not in ledger at block index: {0}
    GiftCodeTxoNotInLedger(u64),

    /// Cannot claim a gift code that has already been claimed
    GiftCodeClaimed,

    /// Cannot claim a gift code which has not yet landed in the ledger
    GiftCodeNotYetAvailable,

    /// Gift Code was removed from the DB prior to claiming
    GiftCodeRemoved,
}

impl From<WalletDbError> for GiftCodeServiceError {
    fn from(src: WalletDbError) -> Self {
        Self::Database(src)
    }
}

impl From<mc_ledger_db::Error> for GiftCodeServiceError {
    fn from(src: mc_ledger_db::Error) -> Self {
        Self::LedgerDB(src)
    }
}

impl From<hex::FromHexError> for GiftCodeServiceError {
    fn from(src: hex::FromHexError) -> Self {
        Self::HexDecode(src)
    }
}

impl From<prost::DecodeError> for GiftCodeServiceError {
    fn from(src: prost::DecodeError) -> Self {
        Self::ProstDecode(src)
    }
}

impl From<diesel::result::Error> for GiftCodeServiceError {
    fn from(src: diesel::result::Error) -> Self {
        Self::Diesel(src)
    }
}

impl From<TransactionServiceError> for GiftCodeServiceError {
    fn from(src: TransactionServiceError) -> Self {
        Self::TransactionService(src)
    }
}

impl From<AccountServiceError> for GiftCodeServiceError {
    fn from(src: AccountServiceError) -> Self {
        Self::AccountService(src)
    }
}

impl From<mc_api::display::Error> for GiftCodeServiceError {
    fn from(src: mc_api::display::Error) -> Self {
        Self::PrintableWrapper(src)
    }
}

impl From<mc_crypto_keys::KeyError> for GiftCodeServiceError {
    fn from(src: mc_crypto_keys::KeyError) -> Self {
        Self::CryptoKey(src)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct EncodedGiftCode(pub String);

impl fmt::Display for EncodedGiftCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The decoded details from the Gift Code.
pub struct DecodedGiftCode {
    root_entropy: RootEntropy,
    txo_public_key: CompressedRistrettoPublic,
    memo: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct GiftCodeEntropy(pub String);

impl fmt::Display for GiftCodeEntropy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Possible states for a Gift Code in relation to accounts in this wallet.
#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum GiftCodeStatus {
    /// The Gift Code has been submitted, but has not yet hit the ledger.
    GiftCodeSubmittedPending,

    /// The Gift Code Txo is in the ledger and has not yet been claimed.
    GiftCodeAvailable,

    /// The Gift Code Txo has been spent.
    GiftCodeClaimed,
}

/// Trait defining the ways in which the wallet can interact with and manage
/// gift codes.
pub trait GiftCodeService {
    /// Builds a new gift code.
    ///
    /// Building a gift code requires the following steps:
    ///  1. Create a new account to receive the funds
    ///  2. Send a transaction to the new account
    ///  3. Wait for the transaction to land
    ///  4. Package the required information into a b58-encoded string
    ///
    /// Returns:
    /// * JsonSubmitResponse from submitting the gift code transaction to the
    ///   network
    /// * Entropy of the gift code account, hex encoded
    #[allow(clippy::too_many_arguments)]
    fn build_gift_code(
        &self,
        from_account_id: &AccountID,
        value: u64,
        name: Option<String>,
        input_txo_ids: Option<&Vec<String>>,
        fee: Option<u64>,
        tombstone_block: Option<u64>,
        max_spendable_value: Option<u64>,
    ) -> Result<(TxProposal, EncodedGiftCode), GiftCodeServiceError>;

    /// Get the details for a specific gift code.
    fn get_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<GiftCode, GiftCodeServiceError>;

    /// List all gift codes in the wallet.
    fn list_gift_codes(&self) -> Result<Vec<GiftCode>, GiftCodeServiceError>;

    /// Check the status of a gift code currently in your wallet. If the gift
    /// code is not yet in the wallet, add it.
    fn check_gift_code_status(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<(GiftCodeStatus, Option<i64>), GiftCodeServiceError>;

    /// Execute a transaction from the gift code account to drain the account to
    /// the destination specified by the account_id_hex and
    /// assigned_subaddress_b58. If no assigned_subaddress_b58 is provided,
    /// then a new AssignedSubaddress will be created to receive the funds.
    fn claim_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
        account_id: &AccountID,
        assigned_subaddress_b58: Option<String>,
    ) -> Result<TransactionLog, GiftCodeServiceError>;

    /// Decode the gift code from b58 to its component parts.
    fn decode_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<DecodedGiftCode, GiftCodeServiceError>;

    fn remove_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<bool, GiftCodeServiceError>;
}

impl<T, FPR> GiftCodeService for WalletService<T, FPR>
where
    T: BlockchainConnection + UserTxConnection + 'static,
    FPR: FogPubkeyResolver + Send + Sync + 'static,
{
    // Implementation: Done
    // Testing: Needs
    fn build_gift_code(
        &self,
        from_account_id: &AccountID,
        value: u64,
        memo: Option<String>,
        input_txo_ids: Option<&Vec<String>>,
        fee: Option<u64>,
        tombstone_block: Option<u64>,
        max_spendable_value: Option<u64>,
    ) -> Result<(TxProposal, EncodedGiftCode), GiftCodeServiceError> {
        // First we need to generate a new random root entropy. The way that gift codes work currently
        // is that the sender creates a middle_man account and sends that account the amount of MOB
        // desired, plus extra to cover the receivers fee
        // Then, that account and all of its secrets get encoded into a b58 string, and when the receiver
        // gets that they can decode it and create a new transaction liquidating the gift account of all
        // of the MOB on its primary account.
        // There should never be a reason to check any other sub_address besides the main one. If there
        // ever is any on a different subaddress, either something went terribly wrong and we messed up,
        // or someone is being very dumb and using a gift account as a place to store their personal MOB.
        let mut rng = rand::thread_rng();
        let gift_code_root_entropy = RootEntropy::from_random(&mut rng);
        let gift_code_account_key = AccountKey::from(&RootIdentity::from(&gift_code_root_entropy));

        // We should never actually need this account to exist in the wallet_db, as we will only ever
        // be using it a single time at this instant with a single unspent txo in its main subaddress
        // and the b58 encoded gc will contain all necessary info to generate a tx_proposal for it
        let gift_code_account_main_subaddress_b58 = b58_encode(&gift_code_account_key.default_subaddress())?;
        
        let from_account = Account::get(&from_account_id, &self.wallet_db.get_conn()?)?;
        
        let tx_proposal = self.build_transaction(
            &from_account.account_id_hex,
            &gift_code_account_main_subaddress_b58,
            value.to_string(),
            input_txo_ids,
            fee.map(|f| f.to_string()),
            tombstone_block.map(|t| t.to_string()),
            max_spendable_value.map(|f| f.to_string()),
        )?;

        if tx_proposal.outlay_index_to_tx_out_index.len() != 1 {
            return Err(GiftCodeServiceError::UnexpectedTxProposalFormat);
        }
        
        let outlay_index = tx_proposal.outlay_index_to_tx_out_index[&0];
        // let value = tx_proposal.outlays[0].value;
        let tx_out = tx_proposal.tx.prefix.outputs[outlay_index].clone();
        let txo_public_key = tx_out.public_key;

        let proto_tx_pubkey: mc_api::external::CompressedRistretto = (&txo_public_key).into();

        let mut gift_code_payload = mc_mobilecoind_api::printable::TransferPayload::new();
        gift_code_payload.set_entropy(gift_code_root_entropy.bytes.to_vec());
        gift_code_payload.set_tx_out_public_key(proto_tx_pubkey);
        gift_code_payload.set_memo(memo.clone().unwrap_or_else(|| "".to_string()));

        let mut gift_code_wrapper = mc_mobilecoind_api::printable::PrintableWrapper::new();
        gift_code_wrapper.set_transfer_payload(gift_code_payload);
        let gift_code_b58 = gift_code_wrapper.b58_encode()?;


        Ok((tx_proposal, EncodedGiftCode(gift_code_b58)))
    }

    /*
    // Implementation: Incomplete
    // Testing: Needs Verification
    fn submit_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
        tx_proposal: &TxProposal
    ) -> Result<GiftCode, GiftCodeServiceError> {
        // TODO: - Requires Implementation
        // We want to officially store the GiftCode into the DB
        // after the transaction has been successfully submitted to the ledger
    }
    */

    // Implementation: Done
    // Testing: Needs Verification
    fn get_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<GiftCode, GiftCodeServiceError> {
        let conn = self.wallet_db.get_conn()?;
        Ok(GiftCode::get(&gift_code_b58, &conn)?)
    }

    // Implementation: Done
    // Testing: Needs Verification
    fn list_gift_codes(&self) -> Result<Vec<GiftCode>, GiftCodeServiceError> {
        let conn = self.wallet_db.get_conn()?;
        Ok(GiftCode::list_all(&conn)?)
    }

    // Implementation: Done
    // Testing: Needs Tests
    fn check_gift_code_status(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<(GiftCodeStatus, Option<i64>), GiftCodeServiceError> {

        log::info!(self.logger, "encoded_gift_code: {:?}", gift_code_b58);

        let decoded_gift_code = self.decode_gift_code(gift_code_b58)?;
        let gift_account_key = AccountKey::from(&RootIdentity::from(&decoded_gift_code.root_entropy));

        log::info!(self.logger, "decoded_gift_code.pubKey: {:?}, account_key: {:?}", decoded_gift_code.txo_public_key, gift_account_key);

        // Check if the GiftCode is in the local ledger.
        let gift_txo = match self
            .ledger_db
            .get_tx_out_index_by_public_key(&decoded_gift_code.txo_public_key)
        {
            Ok(tx_out_index) => self.ledger_db.get_tx_out_by_index(tx_out_index)?,
            Err(mc_ledger_db::Error::NotFound) => {
                return Ok((GiftCodeStatus::GiftCodeSubmittedPending, None))
            }
            Err(e) => return Err(e.into()),
        };

        let shared_secret = get_tx_out_shared_secret(
            gift_account_key.view_private_key(),
            &RistrettoPublic::try_from(&gift_txo.public_key).unwrap(),
        );

        let (value, _blinding) = gift_txo.amount.get_value(&shared_secret).unwrap();

        // Check if the Gift Code has been spent - by convention gift codes are always
        // to the main subaddress index and gift accounts should NEVER have MOB stored
        // anywhere else. If they do, that's not good :,)
        let gift_code_key_image = {
            let onetime_private_key = recover_onetime_private_key(
                &RistrettoPublic::try_from(&decoded_gift_code.txo_public_key)?,
                gift_account_key.view_private_key(),
                &gift_account_key.subaddress_spend_private(DEFAULT_SUBADDRESS_INDEX as u64),
            );
            KeyImage::from(&onetime_private_key)
        };

        if self.ledger_db.contains_key_image(&gift_code_key_image)? {
            return Ok((GiftCodeStatus::GiftCodeClaimed, Some(value as i64)));
        }

        Ok((GiftCodeStatus::GiftCodeAvailable, Some(value as i64)))
    }

    // Implementation: Incomplete
    // Testing: Needs Tests Rewritten - tisk tisk, you should be doing TDD ;)
    fn claim_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
        account_id: &AccountID,
        assigned_subaddress_b58: Option<String>,
    ) -> Result<TransactionLog, GiftCodeServiceError> {
        let (status, gift_code_value) = self.check_gift_code_status(gift_code_b58)?;
        
        match status {
            GiftCodeStatus::GiftCodeClaimed => return Err(GiftCodeServiceError::GiftCodeClaimed),
            GiftCodeStatus::GiftCodeSubmittedPending => {
                return Err(GiftCodeServiceError::GiftCodeNotYetAvailable)
            }
            GiftCodeStatus::GiftCodeAvailable => {}
        }

        let decoded_gift_code = self.decode_gift_code(&gift_code_b58)?;
        let gift_code_account_key = AccountKey::from(&RootIdentity::from(&decoded_gift_code.root_entropy));
        let gift_code_account_id = AccountID::from(&gift_code_account_key);

        let gift_code_account_id_hex = gift_code_account_id.to_string();

        // Checking if we have an already specified destination subaddress, or if we should
        // just use the next available one for the account and tag it with the memo in the
        // decoded_gift_code.
        let destination_address = assigned_subaddress_b58.unwrap_or_else(|| {
            let address = self
                .assign_address_for_account(
                    &account_id,
                    Some(&json!({"gift_code_memo": decoded_gift_code.memo}).to_string()),
                )
                .unwrap();
            address.assigned_subaddress_b58
        });

        // If the gift code value is less than the MINIMUM_FEE, well, then shucks, someone
        // messed up when they were making it. Welcome to the Lost MOB club :)
        if (gift_code_value.unwrap() as u64) < MINIMUM_FEE {
            return Err(GiftCodeServiceError::InsufficientValueForFee(
                gift_code_value.unwrap() as u64,
            ));
        }

        // TODO - This needs to be refactored so that it utilizes mobilecoin::TransactionBuilder
        // instead of WalletTransactionBuilder, as we are no longer relying on EVER importing
        // the gift account into the wallet_db. Again, the thought is, why bother when we don't
        // have to? And also I'm working on the Desktop Wallet and this will solve my problems there
        // so bite me. - Brian.
        let (transaction_log, _associated_txos) = self.build_and_submit(
            &gift_code_account_id_hex,
            &destination_address,
            ((gift_code_value.unwrap() as u64) - MINIMUM_FEE).to_string(),
            None,
            Some(MINIMUM_FEE.to_string()),
            None,
            None,
            Some(
                json!({ "claim_gift_code": decoded_gift_code.memo, "recipient_address": destination_address })
                    .to_string(),
            ),
        )?;

        Ok(transaction_log)
    }

    // Implementation: Done
    // Testing: Needs Verification
    fn decode_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<DecodedGiftCode, GiftCodeServiceError> {
        let wrapper =
            mc_mobilecoind_api::printable::PrintableWrapper::b58_decode(gift_code_b58.to_string())?;
        let transfer_payload = wrapper.get_transfer_payload();

        let mut entropy = [0u8; 32];
        entropy.copy_from_slice(transfer_payload.get_entropy());
        let root_entropy = RootEntropy::from(&entropy);

        let txo_public_key =
            CompressedRistrettoPublic::try_from(transfer_payload.get_tx_out_public_key()).unwrap();

        Ok(DecodedGiftCode {
            root_entropy,
            txo_public_key,
            memo: transfer_payload.get_memo().to_string(),
        })
    }

    // Implementation: Done
    // Testing: Needs Verification
    fn remove_gift_code(
        &self,
        gift_code_b58: &EncodedGiftCode,
    ) -> Result<bool, GiftCodeServiceError> {
        log::info!(self.logger, "Deleting gift code {}", gift_code_b58,);

        let conn = self.wallet_db.get_conn()?;
        GiftCode::get(gift_code_b58, &conn)?.delete(&conn)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{b58_decode, transaction_log::TransactionLogModel},
        service::balance::BalanceService,
        test_utils::{
            add_block_from_transaction_log, add_block_to_ledger_db, add_block_with_tx_proposal,
            get_test_ledger, manually_sync_account, setup_wallet_service, MOB,
        },
    };
    use mc_account_keys::PublicAddress;
    use mc_common::logger::{test_with_logger, Logger};
    use mc_crypto_rand::rand_core::RngCore;
    use mc_transaction_core::ring_signature::KeyImage;
    use rand::{rngs::StdRng, SeedableRng};

    #[test_with_logger]
    fn test_gift_code_lifecycle(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let known_recipients: Vec<PublicAddress> = Vec::new();
        let mut ledger_db = get_test_ledger(5, &known_recipients, 12, &mut rng);

        let service = setup_wallet_service(ledger_db.clone(), logger.clone());

        // Create our main account for the wallet
        let alice = service
            .create_account(Some("Alice's Main Account".to_string()), None)
            .unwrap();

        // Add a block with a transaction for Alice
        let alice_account_key: AccountKey = mc_util_serial::decode(&alice.account_key).unwrap();
        let alice_public_address =
            &alice_account_key.subaddress(alice.main_subaddress_index as u64);
        let alice_account_id = AccountID(alice.account_id_hex.to_string());

        add_block_to_ledger_db(
            &mut ledger_db,
            &vec![alice_public_address.clone()],
            100 * MOB as u64,
            &vec![KeyImage::from(rng.next_u64())],
            &mut rng,
        );
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &alice_account_id,
            13,
            &logger,
        );

        // Verify balance for Alice
        let balance = service
            .get_balance_for_account(&AccountID(alice.account_id_hex.clone()))
            .unwrap();
        assert_eq!(balance.unspent, 100 * MOB as u64);

        // Create a gift code for Bob
        let (tx_proposal, gift_code_b58, _db_gift_code) = service
            .build_gift_code(
                &AccountID(alice.account_id_hex.clone()),
                2 * MOB as u64,
                Some("Gift code for Bob".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        log::info!(logger, "Built and submitted gift code transaction");

        // Check the status before the gift code hits the ledger
        let (status, gift_code_opt) = service
            .check_gift_code_status(&gift_code_b58)
            .expect("Could not get gift code status");
        assert_eq!(status, GiftCodeStatus::GiftCodeSubmittedPending);
        assert!(gift_code_opt.is_none());

        // Now add the block with the tx_proposal
        let transaction_log = TransactionLog::log_submitted(
            tx_proposal.clone(),
            14,
            "Gift Code".to_string(),
            Some(&alice_account_id.to_string()),
            &service.wallet_db.get_conn().unwrap(),
        )
        .expect("Could not log submitted");
        add_block_with_tx_proposal(&mut ledger_db, tx_proposal);
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &alice_account_id,
            14,
            &logger,
        );

        // Now the Gift Code should be Available
        let (status, gift_code_opt) = service
            .check_gift_code_status(&gift_code_b58)
            .expect("Could not get gift code status");
        assert_eq!(status, GiftCodeStatus::GiftCodeAvailable);
        assert!(gift_code_opt.is_some());

        let transaction_recipient =
            b58_decode(&transaction_log.recipient_public_address_b58).unwrap();

        let decoded = service
            .decode_gift_code(&gift_code_b58)
            .expect("Could not decode gift code");
        let gift_code_account_key = AccountKey::from(&RootIdentity::from(&decoded.root_entropy));
        let gift_code_public_address = gift_code_account_key.default_subaddress();

        assert_eq!(gift_code_public_address, transaction_recipient);

        // Get the tx_out from the ledger and check that it matches expectations
        log::info!(logger, "Retrieving gift code Txo from ledger");
        let tx_out_index = ledger_db
            .get_tx_out_index_by_public_key(&decoded.txo_public_key)
            .unwrap();
        let tx_out = ledger_db.get_tx_out_by_index(tx_out_index).unwrap();
        let shared_secret = get_tx_out_shared_secret(
            gift_code_account_key.view_private_key(),
            &RistrettoPublic::try_from(&tx_out.public_key).unwrap(),
        );
        let (value, _blinding) = tx_out.amount.get_value(&shared_secret).unwrap();
        assert_eq!(value, 2000000000000);

        // Verify balance for Alice = original balance - fee - gift_code_value
        let balance = service
            .get_balance_for_account(&AccountID(alice.account_id_hex.clone()))
            .unwrap();
        assert_eq!(balance.unspent, 97990000000000);

        // Verify that we can get the gift_code
        log::info!(logger, "Getting gift code from database");
        let gotten_gift_code = service.get_gift_code(&gift_code_b58).unwrap();
        assert_eq!(gotten_gift_code.value, value as i64);
        assert_eq!(gotten_gift_code.gift_code_b58, gift_code_b58.to_string());

        // Check that we can list all
        log::info!(logger, "Listing all gift codes");
        let gift_codes = service.list_gift_codes().unwrap();
        assert_eq!(gift_codes.len(), 1);
        assert_eq!(gift_codes[0], gotten_gift_code);

        // Hack to make sure the gift code account has scanned the gift code Txo -
        // otherwise claim_gift_code hangs.
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &AccountID::from(&gift_code_account_key),
            14,
            &logger,
        );

        // Claim the gift code to another account
        log::info!(logger, "Creating new account to receive gift code");
        let bob = service
            .create_account(Some("Bob's Main Account".to_string()), None)
            .unwrap();
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &AccountID(bob.account_id_hex.clone()),
            14,
            &logger,
        );

        log::info!(logger, "Claiming gift code");
        let (consume_response, _gift_code) = service
            .claim_gift_code(&gift_code_b58, &AccountID(bob.account_id_hex.clone()), None)
            .unwrap();

        // Add the consume transaction to the ledger
        log::info!(
            logger,
            "Adding block to ledger with consume gift code transaction"
        );
        {
            let conn = service.wallet_db.get_conn().unwrap();
            let consume_transaction_log =
                TransactionLog::get(&consume_response.transaction_id_hex, &conn).unwrap();
            add_block_from_transaction_log(&mut ledger_db, &conn, &consume_transaction_log);
        };
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &AccountID(bob.account_id_hex.clone()),
            15,
            &logger,
        );

        // Now the Gift Code should be spent
        let (status, gift_code_opt) = service
            .check_gift_code_status(&gift_code_b58)
            .expect("Could not get gift code status");
        assert_eq!(status, GiftCodeStatus::GiftCodeClaimed);
        assert!(gift_code_opt.is_some());

        // Bob's balance should be = gift code value - fee (10000000000)
        let bob_balance = service
            .get_balance_for_account(&AccountID(bob.account_id_hex))
            .unwrap();
        assert_eq!(bob_balance.unspent, 1990000000000)
    }

    #[test_with_logger]
    fn test_remove_gift_code(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let known_recipients: Vec<PublicAddress> = Vec::new();
        let mut ledger_db = get_test_ledger(5, &known_recipients, 12, &mut rng);

        let service = setup_wallet_service(ledger_db.clone(), logger.clone());

        // Create our main account for the wallet
        let alice = service
            .create_account(Some("Alice's Main Account".to_string()), None)
            .unwrap();

        // Add a block with a transaction for Alice
        let alice_account_key: AccountKey = mc_util_serial::decode(&alice.account_key).unwrap();
        let alice_public_address =
            &alice_account_key.subaddress(alice.main_subaddress_index as u64);
        let alice_account_id = AccountID(alice.account_id_hex.to_string());

        add_block_to_ledger_db(
            &mut ledger_db,
            &vec![alice_public_address.clone()],
            100 * MOB as u64,
            &vec![KeyImage::from(rng.next_u64())],
            &mut rng,
        );
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &alice_account_id,
            13,
            &logger,
        );

        // Verify balance for Alice
        let balance = service
            .get_balance_for_account(&AccountID(alice.account_id_hex.clone()))
            .unwrap();
        assert_eq!(balance.unspent, 100 * MOB as u64);

        // Create a gift code for Bob
        let (tx_proposal, gift_code_b58, _db_gift_code) = service
            .build_gift_code(
                &AccountID(alice.account_id_hex.clone()),
                2 * MOB as u64,
                Some("Gift code for Bob".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        log::info!(logger, "Built and submitted gift code transaction");

        // Check the status before the gift code hits the ledger
        let (status, gift_code_opt) = service
            .check_gift_code_status(&gift_code_b58)
            .expect("Could not get gift code status");
        assert_eq!(status, GiftCodeStatus::GiftCodeSubmittedPending);
        assert!(gift_code_opt.is_none());

        // Let gift code hit the ledger
        let _transaction_log = TransactionLog::log_submitted(
            tx_proposal.clone(),
            14,
            "Gift Code".to_string(),
            Some(&alice_account_id.to_string()),
            &service.wallet_db.get_conn().unwrap(),
        )
        .expect("Could not log submitted");
        add_block_with_tx_proposal(&mut ledger_db, tx_proposal);
        manually_sync_account(
            &ledger_db,
            &service.wallet_db,
            &alice_account_id,
            14,
            &logger,
        );

        // Check that it landed
        let (status, gift_code_opt) = service
            .check_gift_code_status(&gift_code_b58)
            .expect("Could not get gift code status");
        assert_eq!(status, GiftCodeStatus::GiftCodeAvailable);
        assert!(gift_code_opt.is_some());

        // Check that we get all gift codes
        let gift_codes = service
            .list_gift_codes()
            .expect("Could not list gift codes");
        assert_eq!(gift_codes.len(), 1);

        assert_eq!(gift_codes[0], gift_code_opt.unwrap());

        // remove that gift code
        assert!(service
            .remove_gift_code(&gift_code_b58)
            .expect("Could not remove gift code"));
        let gift_codes = service
            .list_gift_codes()
            .expect("Could not list gift codes");
        assert_eq!(gift_codes.len(), 0);
    }
}
