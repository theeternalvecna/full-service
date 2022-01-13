// Copyright (c) 2018-2022 MobileCoin, Inc.

//! Ledger syncing via the Validator Service.

use mc_common::logger::{log, Logger};
use mc_ledger_db::{Ledger, LedgerDB};
use mc_transaction_core::{Block, BlockContents};
use mc_validator_api::ValidatorUri;
use mc_validator_connection::ValidatorConnection;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// The maximum number of blocks to try and retrieve in each iteration
pub const MAX_BLOCKS_PER_SYNC_ITERATION: u32 = 1000;

pub struct ValidatorLedgerSyncThread {
    join_handle: Option<thread::JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
}

impl ValidatorLedgerSyncThread {
    pub fn new(
        validator_uri: &ValidatorUri,
        poll_interval: Duration,
        ledger_db: LedgerDB,
        logger: Logger,
    ) -> Self {
        let stop_requested = Arc::new(AtomicBool::new(false));

        let validator_conn = ValidatorConnection::new(validator_uri, logger.clone());

        let thread_stop_requested = stop_requested.clone();
        let join_handle = Some(
            thread::Builder::new()
                .name("ValidatorLedgerSync".into())
                .spawn(move || {
                    Self::thread_entrypoint(
                        validator_conn,
                        poll_interval,
                        ledger_db,
                        logger,
                        thread_stop_requested,
                    );
                })
                .expect("Failed spawning ValidatorLedgerSync thread"),
        );

        Self {
            join_handle,
            stop_requested,
        }
    }

    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(thread) = self.join_handle.take() {
            thread.join().expect("thread join failed");
        }
    }

    fn thread_entrypoint(
        validator_conn: ValidatorConnection,
        poll_interval: Duration,
        mut ledger_db: LedgerDB,
        logger: Logger,
        stop_requested: Arc<AtomicBool>,
    ) {
        log::info!(logger, "ValidatorLedgerSync thread started");

        loop {
            if stop_requested.load(Ordering::SeqCst) {
                log::debug!(logger, "ValidatorLedgerSyncThread stop requested.");
                break;
            }

            let blocks_and_contents = Self::get_next_blocks(&ledger_db, &validator_conn, &logger);
            if !blocks_and_contents.is_empty() {
                Self::append_safe_blocks(&mut ledger_db, &blocks_and_contents, &logger);
            }

            // If we got no blocks, or less than the amount we asked for, sleep for a bit.
            // Getting less the amount we asked for indicates we are fully synced.
            if blocks_and_contents.is_empty()
                || blocks_and_contents.len() < MAX_BLOCKS_PER_SYNC_ITERATION as usize
            {
                thread::sleep(poll_interval);
            }
        }
    }

    fn get_next_blocks(
        ledger_db: &LedgerDB,
        validator_conn: &ValidatorConnection,
        logger: &Logger,
    ) -> Vec<(Block, BlockContents)> {
        let num_blocks = ledger_db
            .num_blocks()
            .expect("Failed getting the number of blocks in ledger");

        let blocks_data =
            match validator_conn.get_blocks_data(num_blocks, MAX_BLOCKS_PER_SYNC_ITERATION) {
                Ok(blocks_data) => blocks_data,
                Err(err) => {
                    log::error!(
                        logger,
                        "Failed getting blocks data from validator: {:?}",
                        err
                    );
                    return Vec::new();
                }
            };

        let blocks_and_contents: Vec<(Block, BlockContents)> = blocks_data
            .into_iter()
            .map(|block_data| (block_data.block().clone(), block_data.contents().clone()))
            .collect();

        match mc_ledger_sync::identify_safe_blocks(ledger_db, &blocks_and_contents, logger) {
            Ok(safe_blocks) => safe_blocks,
            Err(err) => {
                log::error!(logger, "Failed identifying safe blocks: {:?}", err);
                Vec::new()
            }
        }
    }

    fn append_safe_blocks(
        ledger_db: &mut LedgerDB,
        blocks_and_contents: &[(Block, BlockContents)],
        logger: &Logger,
    ) {
        log::info!(
            logger,
            "Appending {} blocks to ledger, which currently has {} blocks",
            blocks_and_contents.len(),
            ledger_db
                .num_blocks()
                .expect("failed getting number of blocks"),
        );

        for (block, contents) in blocks_and_contents {
            ledger_db
                .append_block(block, contents, None)
                .expect(&format!(
                    "Failed appending block #{} to ledger",
                    block.index
                ));
        }
    }
}

impl Drop for ValidatorLedgerSyncThread {
    fn drop(&mut self) {
        self.stop();
    }
}
