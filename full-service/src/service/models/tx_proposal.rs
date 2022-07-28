use std::convert::{TryFrom, TryInto};

use mc_account_keys::PublicAddress;
use mc_transaction_core::{
    ring_signature::KeyImage,
    tokens::Mob,
    tx::{Tx, TxOut, TxOutConfirmationNumber},
    Amount, Token,
};

use crate::util::b58::b58_decode_public_address;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputTxo {
    pub tx_out: TxOut,
    pub subaddress_index: u64,
    pub key_image: KeyImage,
    pub amount: Amount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputTxo {
    pub tx_out: TxOut,
    pub recipient_public_address: PublicAddress,
    pub confirmation_number: TxOutConfirmationNumber,
    pub amount: Amount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxProposal {
    pub tx: Tx,
    pub input_txos: Vec<InputTxo>,
    pub payload_txos: Vec<OutputTxo>,
    pub change_txos: Vec<OutputTxo>,
}

impl TryFrom<&crate::json_rpc::v1::models::tx_proposal::TxProposal> for TxProposal {
    type Error = String;

    fn try_from(
        src: &crate::json_rpc::v1::models::tx_proposal::TxProposal,
    ) -> Result<Self, Self::Error> {
        let mc_api_tx = mc_api::external::Tx::try_from(&src.tx).map_err(|e| e.to_string())?;
        let tx = Tx::try_from(&mc_api_tx).map_err(|e| e.to_string())?;

        let input_txos = src
            .input_list
            .iter()
            .map(|unspent_txo| {
                let mc_api_tx_out = mc_api::external::TxOut::try_from(&unspent_txo.tx_out)
                    .map_err(|e| e.to_string())?;
                let tx_out = TxOut::try_from(&mc_api_tx_out).map_err(|e| e.to_string())?;

                let key_image_bytes =
                    hex::decode(unspent_txo.key_image.clone()).map_err(|e| e.to_string())?;
                let key_image =
                    KeyImage::try_from(key_image_bytes.as_slice()).map_err(|e| e.to_string())?;

                Ok(InputTxo {
                    tx_out,
                    subaddress_index: unspent_txo
                        .subaddress_index
                        .parse::<u64>()
                        .map_err(|e| e.to_string())?,
                    key_image,
                    amount: Amount::new(unspent_txo.value, Mob::ID),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let mut payload_txos = Vec::new();

        for (outlay_index, tx_out_index) in src.outlay_index_to_tx_out_index.iter() {
            let outlay_index = outlay_index.parse::<usize>().map_err(|e| e.to_string())?;
            let outlay = &src.outlay_list[outlay_index];
            let tx_out_index = tx_out_index.parse::<usize>().map_err(|e| e.to_string())?;
            let tx_out = tx.prefix.outputs[tx_out_index].clone();
            let confirmation_number_bytes: &[u8; 32] = src.outlay_confirmation_numbers
                [outlay_index]
                .as_slice()
                .try_into()
                .map_err(|_| {
                    "confirmation number is not the right number of bytes (expecting 32)"
                        .to_string()
                })?;

            let confirmation_number = TxOutConfirmationNumber::from(confirmation_number_bytes);

            let mc_api_public_address = mc_api::external::PublicAddress::try_from(&outlay.receiver)
                .map_err(|e| e.to_string())?;
            let public_address =
                PublicAddress::try_from(&mc_api_public_address).map_err(|e| e.to_string())?;

            let payload_txo = OutputTxo {
                tx_out,
                recipient_public_address: public_address,
                confirmation_number,
                amount: Amount::new(outlay.value.0, Mob::ID),
            };

            payload_txos.push(payload_txo);
        }

        Ok(Self {
            tx,
            input_txos,
            payload_txos,
            change_txos: Vec::new(),
        })
    }
}

impl TryFrom<&crate::json_rpc::v2::models::tx_proposal::TxProposal> for TxProposal {
    type Error = String;

    fn try_from(
        src: &crate::json_rpc::v2::models::tx_proposal::TxProposal,
    ) -> Result<Self, Self::Error> {
        let tx_bytes = hex::decode(&src.tx_proto).map_err(|e| e.to_string())?;
        let tx = mc_util_serial::decode(tx_bytes.as_slice()).map_err(|e| e.to_string())?;
        let input_txos = src
            .input_txos
            .iter()
            .map(|input_txo| {
                let key_image_bytes =
                    hex::decode(&input_txo.key_image).map_err(|e| e.to_string())?;
                Ok(InputTxo {
                    tx_out: mc_util_serial::decode(
                        hex::decode(&input_txo.tx_out_proto)
                            .map_err(|e| e.to_string())?
                            .as_slice(),
                    )
                    .map_err(|e| e.to_string())?,
                    subaddress_index: input_txo
                        .subaddress_index
                        .parse::<u64>()
                        .map_err(|e| e.to_string())?,
                    key_image: KeyImage::try_from(key_image_bytes.as_slice())
                        .map_err(|e| e.to_string())?,
                    amount: Amount::try_from(&input_txo.amount).map_err(|e| e.to_string())?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let payload_txos = src
            .payload_txos
            .iter()
            .map(|payload_txo| {
                let confirmation_number_bytes: &[u8; 32] = &hex::decode(&payload_txo.tx_out_proto)
                    .map_err(|e| e.to_string())?
                    .as_slice()
                    .try_into()
                    .map_err(|_| {
                        "confirmation number is not the right number of bytes (expecting 32)"
                            .to_string()
                    })?;
                Ok(OutputTxo {
                    tx_out: mc_util_serial::decode(
                        hex::decode(&payload_txo.tx_out_proto)
                            .map_err(|e| e.to_string())?
                            .as_slice(),
                    )
                    .map_err(|e| e.to_string())?,
                    recipient_public_address: b58_decode_public_address(
                        &payload_txo.recipient_public_address_b58,
                    )
                    .map_err(|e| e.to_string())?,
                    confirmation_number: TxOutConfirmationNumber::from(confirmation_number_bytes),
                    amount: Amount::try_from(&payload_txo.amount).map_err(|e| e.to_string())?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let change_txos = src
            .change_txos
            .iter()
            .map(|change_txo| {
                let confirmation_number_bytes: &[u8; 32] = &hex::decode(&change_txo.tx_out_proto)
                    .map_err(|e| e.to_string())?
                    .as_slice()
                    .try_into()
                    .map_err(|_| {
                        "confirmation number is not the right number of bytes (expecting 32)"
                            .to_string()
                    })?;
                Ok(OutputTxo {
                    tx_out: mc_util_serial::decode(
                        hex::decode(&change_txo.tx_out_proto)
                            .map_err(|e| e.to_string())?
                            .as_slice(),
                    )
                    .map_err(|e| e.to_string())?,
                    recipient_public_address: b58_decode_public_address(
                        &change_txo.recipient_public_address_b58,
                    )
                    .map_err(|e| e.to_string())?,
                    confirmation_number: TxOutConfirmationNumber::from(confirmation_number_bytes),
                    amount: Amount::try_from(&change_txo.amount).map_err(|e| e.to_string())?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            tx,
            input_txos,
            payload_txos,
            change_txos,
        })
    }
}
