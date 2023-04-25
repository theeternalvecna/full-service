// Copyright (c) 2020-2023 MobileCoin Inc.

//! The Gift Code Model.

use crate::{
    db::{
        models::{GiftCode, NewGiftCode},
        Conn, WalletDbError,
    },
    service::gift_code::EncodedGiftCode,
};
use diesel::prelude::*;
use displaydoc::Display;

#[derive(Display, Debug)]
pub enum GiftCodeDbError {
    /// Could not get gift code: {0}
    GiftCodeNotFound(String),
}

#[rustfmt::skip]
pub trait GiftCodeModel {
    /// Create a gift code.
    ///
    /// This method should be called after the account has already
    /// been inserted into the DB, the txo has already been deposited to
    /// that account, and the transaction_log has been stored for that
    /// deposit, all of which are handled by the GiftCodeService.
    /// 
    /// # Arguments
    /// 
    ///| Name            | Purpose                                                | Notes                                                      |
    ///|-----------------|--------------------------------------------------------|------------------------------------------------------------|
    ///| `gift_code_b58` | The base58-encoded gift code contents.                 | Gift code includes `entropy`, `txo public key`, and `memo` |
    ///| `value`         | The amount of MOB to send in this transaction.         |                                                            |
    ///| `conn`          | An reference to the pool connection of wallet database |                                                            |
    ///
    /// # Returns:
    /// * Gift code encoded as b58 string.
    #[allow(clippy::too_many_arguments)]
    fn create(
        gift_code_b58: &EncodedGiftCode,
        value: i64,
        conn: Conn,
    ) -> Result<GiftCode, WalletDbError>;

    /// Get the details of a specific Gift Code.
    /// 
    /// # Arguments
    /// 
    ///| Name            | Purpose                                                | Notes                 |
    ///|-----------------|--------------------------------------------------------|-----------------------|
    ///| `gift_code_b58` | The base58-encoded gift code contents.                 | Gift code must exist. |
    ///| `conn`          | An reference to the pool connection of wallet database |                       |
    /// 
    /// # Returns:
    /// * Gift code encoded as b58 string.
    fn get(
        gift_code_b58: &EncodedGiftCode, 
        conn: Conn
    ) -> Result<GiftCode, WalletDbError>;

    /// Get all Gift Codes in this wallet.
    /// 
    /// # Arguments
    /// 
    ///| Name     | Purpose                                                   | Notes                    |
    ///|----------|-----------------------------------------------------------|--------------------------|
    ///| `conn`   | An reference to the pool connection of wallet database    |                          |
    ///| `offset` | The pagination offset. Results start at the offset index. | Optional, defaults to 0. |
    ///| `limit`  | Limit for the number of results.                          | Optional                 |
    ///
    /// # Returns:
    /// * Vector of Gift code encoded as b58 string.
    fn list_all(
        conn: Conn,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Vec<GiftCode>, WalletDbError>;

    /// Delete a gift code.
    /// 
    /// # Arguments
    /// 
    ///| Name     | Purpose                                                   | Notes                    |
    ///|----------|-----------------------------------------------------------|--------------------------|
    ///| `conn`   | An reference to the pool connection of wallet database    |                          |
    ///
    /// # Returns:
    /// * unit
    fn delete(self, conn: Conn) -> Result<(), WalletDbError>;
}

impl GiftCodeModel for GiftCode {
    fn create(
        gift_code_b58: &EncodedGiftCode,
        value: i64,
        conn: Conn,
    ) -> Result<GiftCode, WalletDbError> {
        use crate::db::schema::gift_codes;

        // Insert the gift code to our gift code table.
        let new_gift_code = NewGiftCode {
            gift_code_b58: &gift_code_b58.to_string(),
            value,
        };
        diesel::insert_into(gift_codes::table)
            .values(&new_gift_code)
            .execute(conn)?;

        let gift_code = GiftCode::get(gift_code_b58, conn)?;
        Ok(gift_code)
    }

    fn get(gift_code_b58: &EncodedGiftCode, conn: Conn) -> Result<GiftCode, WalletDbError> {
        use crate::db::schema::gift_codes::dsl::{gift_code_b58 as dsl_gift_code_b58, gift_codes};

        match gift_codes
            .filter(dsl_gift_code_b58.eq(gift_code_b58.to_string()))
            .get_result::<GiftCode>(conn)
        {
            Ok(a) => Ok(a),
            // Match on NotFound to get a more informative NotFound Error
            Err(diesel::result::Error::NotFound) => {
                Err(GiftCodeDbError::GiftCodeNotFound(gift_code_b58.to_string()).into())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn list_all(
        conn: Conn,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Vec<GiftCode>, WalletDbError> {
        use crate::db::schema::gift_codes;

        let mut query = gift_codes::table.into_boxed();

        if let (Some(offset), Some(limit)) = (offset, limit) {
            query = query.offset(offset as i64).limit(limit as i64);
        }

        Ok(query.load(conn)?)
    }

    fn delete(self, conn: Conn) -> Result<(), WalletDbError> {
        use crate::db::schema::gift_codes::dsl::{gift_code_b58, gift_codes};

        diesel::delete(gift_codes.filter(gift_code_b58.eq(&self.gift_code_b58))).execute(conn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{create_test_txo_for_recipient, WalletDbTestContext};
    use mc_account_keys::{AccountKey, RootIdentity};
    use mc_common::logger::{test_with_logger, Logger};
    use mc_rand::rand_core::RngCore;
    use mc_transaction_core::{tokens::Mob, Amount, Token};
    use mc_util_from_random::FromRandom;
    use rand::{rngs::StdRng, SeedableRng};

    // Basic test of gift codes in database.
    #[test_with_logger]
    fn test_gift_code_crud(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([20u8; 32]);

        let db_test_context = WalletDbTestContext::default();
        let wallet_db = db_test_context.get_db_instance(logger);

        let root_identity = RootIdentity::from_random(&mut rng);
        let gift_code_account_key = AccountKey::from(&root_identity);

        // Note: This value isn't actually associated with the txo_public_key, but is
        // sufficient for this test to merely log a value.
        let value = rng.next_u64();

        let (_tx_out, _key_image) = create_test_txo_for_recipient(
            &gift_code_account_key,
            0,
            Amount::new(value, Mob::ID),
            &mut rng,
        );

        let mut tx_log_bytes = [0u8; 32];
        rng.fill_bytes(&mut tx_log_bytes);

        let gift_code = GiftCode::create(
            &EncodedGiftCode("gk7CcXuK5RKNW13LvrWY156ZLjaoHaXxLedqACZsw3w6FfF6TR4TVzaAQkH5EHxaw54DnGWRJPA31PpcmvGLoArZbDRj1kBhcTusE8AVW4Mj7QT5".to_string()),
            value as i64,
            &mut wallet_db.get_pooled_conn().unwrap(),
        )
        .unwrap();

        let gotten = GiftCode::get(
            &EncodedGiftCode(gift_code.gift_code_b58),
            &mut wallet_db.get_pooled_conn().unwrap(),
        )
        .unwrap();

        let expected_gift_code = GiftCode {
            id: 1,
            gift_code_b58: gotten.gift_code_b58.clone(),
            value: value as i64,
        };
        assert_eq!(gotten, expected_gift_code);

        let all_gift_codes =
            GiftCode::list_all(&mut wallet_db.get_pooled_conn().unwrap(), None, None).unwrap();
        assert_eq!(all_gift_codes.len(), 1);
        assert_eq!(all_gift_codes[0], expected_gift_code);
    }
}
