// Copyright (c) 2020-2021 MobileCoin Inc.

//! JSON-RPC Responses from the Wallet API.
//!
//! API v2

use crate::{
    json_rpc::{account::Account, api_v1::decorated_types::JsonAccount},
    service::WalletService,
};
use mc_connection::{BlockchainConnection, UserTxConnection};
use mc_fog_report_validation::FogPubkeyResolver;
use rocket_contrib::json::Json;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

/// A JSON RPC Response.
#[derive(Deserialize, Serialize, Debug)]
pub struct JsonRPCResponse {
    /// The method which was invoked on the server.
    ///
    /// Optional because JSON RPC does not require returning the method invoked,
    /// as that can be determined by the id. We return it as a convenience.
    pub method: Option<String>,

    /// The result of invoking the method on the server.
    ///
    /// Optional: if error occurs, result is not returned.
    pub result: Option<serde_json::Value>,

    /// The error that occurred when invoking the method on the server.
    ///
    /// Optional: if method was successful, error is not returned.
    pub error: Option<JsonRPCError>,

    /// The JSON RPC version. Should always be 2.0.
    pub jsonrpc: String,

    /// The id of the Request object to which this response corresponds.
    pub id: String,
}

impl From<JsonCommandResponseV2> for JsonRPCResponse {
    fn from(src: JsonCommandResponseV2) -> JsonRPCResponse {
        let json_response = json!(src);
        JsonRPCResponse {
            method: Some(json_response.get("method").unwrap().to_string()),
            result: Some(json_response.get("result").unwrap().clone()),
            error: None,
            jsonrpc: "2.0".to_string(),
            id: "1".to_string(),
        }
    }
}

/// A JSON RPC Error.
#[derive(Deserialize, Serialize, Debug)]
#[serde(untagged)]
#[allow(non_camel_case_types)]
pub enum JsonRPCError {
    error {
        /// The error code associated with this error.
        code: JsonRPCErrorCodes,

        /// A string providing a short description of the error.
        message: String,

        /// Additional information about the error.
        data: String, // FIXME: could be json structured value.
    },
}

/// JSON RPC Error codes.
#[derive(Deserialize, Serialize, Debug)]
pub enum JsonRPCErrorCodes {
    /// Parse error.
    ParseError = -32700,

    /// Invalid request.
    InvalidRequest = -32600,

    /// Method not found.
    MethodNotFound = -32601,

    /// Invalid params.
    InvalidParams = -32602,

    /// Internal Error.
    InternalError = -32603,
    /* Server error.
     * ServerError(i32), // FIXME: WalletServiceError -> i32 between 32000 and 32099 */
}

/// Helper method to format displaydoc errors in JSON RPC 2.0 format.
pub fn format_error<T: std::fmt::Display + std::fmt::Debug>(e: T) -> String {
    let data = json!({"server_error": format!("{:?}", e), "details": e.to_string()}).to_string();
    // FIXME: wrap in JsonRPCResponse
    let json_resp = JsonRPCError::error {
        code: JsonRPCErrorCodes::InternalError,
        message: "Internal error".to_string(),
        data,
    };
    json!(json_resp).to_string()
}

/// Responses from the Full Service Wallet.
#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "method", content = "result")]
#[allow(non_camel_case_types)]
pub enum JsonCommandResponseV2 {
    create_account { account: Account },
}
