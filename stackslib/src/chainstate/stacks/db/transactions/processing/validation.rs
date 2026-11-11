// Copyright (C) 2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Static validation performed before transaction execution.

use crate::chainstate::stacks::db::DBConfig;
use crate::chainstate::stacks::{
    Error, StacksTransaction, TransactionAuthVerificationMode, TransactionPayload,
    TransactionVersion,
};
use crate::core::StacksEpochId;

/// Pre-check a transaction -- make sure it's well-formed.
///
/// Besides the epoch gates in [`check_transaction_epoch_gates`], this checks what only applies
/// when a single transaction is admitted or processed on its own:
/// - the auth mode is supported in `epoch_id`;
/// - the signatures verify (high-S signatures are only allowed in some epochs);
/// - the chain ID and mainnet/testnet version match `config`.
///
/// Static block validation ([`crate::chainstate::stacks::StacksBlock::validate_transaction_static_epoch`])
/// additionally gates coinbase, pre-2.1 versioned deploys and `TenureChange` payloads. Those gates
/// are deliberately not applied here: Epoch 2.x microblock transactions get no other per-transaction
/// epoch check, so adding them here could change which historical transactions are accepted.
pub fn precheck_transaction(
    config: &DBConfig,
    tx: &StacksTransaction,
    epoch_id: StacksEpochId,
) -> Result<(), Error> {
    // valid auth?
    if !tx.auth.is_supported_in_epoch(epoch_id) {
        let msg = format!(
            "Invalid tx {}: authentication mode not supported in Epoch {epoch_id}",
            tx.txid()
        );
        warn!("{msg}");

        return Err(Error::InvalidStacksTransaction(msg, false));
    }
    let verification_mode = if epoch_id.allows_tx_signatures_with_high_s() {
        TransactionAuthVerificationMode::AllowHighS
    } else {
        TransactionAuthVerificationMode::EnforceLowS
    };

    tx.verify(verification_mode)?;

    // destined for us?
    if config.chain_id != tx.chain_id {
        let msg = format!(
            "Invalid tx {}: invalid chain ID {} (expected {})",
            tx.txid(),
            tx.chain_id,
            config.chain_id
        );
        warn!("{}", &msg);

        return Err(Error::InvalidStacksTransaction(msg, false));
    }

    match tx.version {
        TransactionVersion::Mainnet => {
            if !config.mainnet {
                let msg = format!("Invalid tx {}: on testnet; got mainnet", tx.txid());
                warn!("{}", &msg);

                return Err(Error::InvalidStacksTransaction(msg, false));
            }
        }
        TransactionVersion::Testnet => {
            if config.mainnet {
                let msg = format!("Invalid tx {}: on mainnet; got testnet", tx.txid());
                warn!("{}", &msg);

                return Err(Error::InvalidStacksTransaction(msg, false));
            }
        }
    }

    // The remaining epoch gates are shared with static block validation.
    check_transaction_epoch_gates(tx, epoch_id).map_err(|msg| {
        info!("{msg}"; "txid" => %tx.txid());
        Error::InvalidStacksTransaction(msg, false)
    })?;

    Ok(())
}

/// Epoch gates on transaction content that both [`precheck_transaction`] and static block
/// validation apply. New content gates that must hold on both paths belong here.
///
/// Checks, in order: post-condition support, versioned-deploy support (only for
/// `SmartContract(_, Some(_))`) and contract-name support.
///
/// # Errors
///
/// Returns the message to report: `"Invalid Stacks transaction: {reason}"` for post-conditions,
/// and `"Invalid transaction {txid}: {reason}"` for versioned deploys and contract names.
///
/// # Panics
///
/// A versioned deploy reaches `check_versioned_deploy_supported_in_epoch`, which panics at
/// Epoch 1.0 (no Clarity exists there). Callers must reject versioned deploys before Epoch 2.1
/// first, or only pass epochs where Clarity exists.
pub(crate) fn check_transaction_epoch_gates(
    tx: &StacksTransaction,
    epoch_id: StacksEpochId,
) -> Result<(), String> {
    stacks_transactions::check_post_conditions_supported_in_epoch(
        &tx.post_conditions,
        &tx.post_condition_mode,
        epoch_id,
    )
    .map_err(|reason| format!("Invalid Stacks transaction: {reason}"))?;

    if let TransactionPayload::SmartContract(_, Some(clarity_version)) = &tx.payload {
        stacks_transactions::check_versioned_deploy_supported_in_epoch(*clarity_version, epoch_id)
            .map_err(|reason| format!("Invalid transaction {}: {reason}", tx.txid()))?;
    }

    stacks_transactions::check_contract_names_supported_in_epoch(
        &tx.payload,
        &tx.post_conditions,
        epoch_id,
    )
    .map_err(|reason| format!("Invalid transaction {}: {reason}", tx.txid()))?;

    Ok(())
}
