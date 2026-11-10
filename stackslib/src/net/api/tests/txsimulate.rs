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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use clarity::types::chainstate::StacksPrivateKey;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::types::PrincipalData;
use clarity::vm::{ClarityName, ContractName, Value};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::chainstate::StacksBlockId;

use crate::chainstate::nakamoto::NakamotoChainState;
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::StacksTransaction;
use crate::core::test_util::{
    make_contract_call_tx, make_contract_publish_tx, make_stacks_transfer_tx, to_addr,
};
use crate::core::MemPoolDB;
use crate::net::api::tests::TestRPC;
use crate::net::api::txsimulate::{
    self, RPCMempoolConflict, RPCMempoolConflictOutcome, TxSimulateError,
};
use crate::net::connection::ConnectionOptions;
use crate::net::httpcore::{StacksHttp, StacksHttpRequest};
use crate::net::test::TestEventObserver;
use crate::net::tests::{NakamotoBootStep, NakamotoBootTenure};
use crate::net::{ProtocolFamily, TipRequest};

#[test]
fn test_try_parse_request() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let private_key = StacksPrivateKey::from_seed("txsimulate".as_bytes());
    let tx = make_contract_publish_tx(
        &private_key,
        0,
        1000,
        CHAIN_ID_TESTNET,
        "print-contract",
        "(print u1)",
        Some(clarity::vm::ClarityVersion::Clarity1),
    );

    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &tx,
        TipRequest::UseLatestAnchoredTip,
    );

    // add the authorization header
    request.add_header("authorization".into(), "password".into());

    let bytes = request.try_serialize().unwrap();

    debug!("Request:\n{}\n", std::str::from_utf8(&bytes).unwrap());

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();

    let mut handler =
        txsimulate::RPCTransactionSimulateRequestHandler::new(Some("password".into()));

    let mut parsed_request = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap();
    assert_eq!(handler.transaction, Some(tx));

    // parsed request consumes headers that would not be in a constructed request
    parsed_request.clear_headers();
    parsed_request.add_header("authorization".into(), "password".into());

    let (preamble, _contents) = parsed_request.destruct();

    assert_eq!(&preamble, request.preamble());
}

/// Return the index block hash of the highest epoch-2.x block in this
/// chainstate.  The Nakamoto boot plan mines an epoch-2.x chain before
/// activating Nakamoto, so these blocks are present in the headers DB and can
/// be named by a client via the `tip` query parameter -- but they cannot be
/// extended by the simulation endpoint, which builds a Nakamoto block.
fn last_epoch2_block_id(chainstate: &StacksChainState) -> StacksBlockId {
    chainstate
        .db()
        .query_row(
            "SELECT index_block_hash FROM block_headers ORDER BY block_height DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("FATAL: no epoch-2.x blocks in the test chainstate")
}

#[test]
fn test_transaction_simulate_errors() {
    let test_observer = TestEventObserver::new();
    let mut rpc_test = TestRPC::setup_nakamoto(function_name!(), &test_observer);

    let epoch2_tip = last_epoch2_block_id(rpc_test.peer_1.chainstate_ref());

    let sort_db = rpc_test.peer_1.chain.sortdb.take().unwrap();
    let chainstate = rpc_test.peer_1.chainstate();

    let private_key = StacksPrivateKey::from_seed("txsimulate".as_bytes());
    let tx = make_contract_publish_tx(
        &private_key,
        0,
        1000,
        CHAIN_ID_TESTNET,
        "print-contract",
        "(print u1)",
        Some(clarity::vm::ClarityVersion::Clarity1),
    );

    // non-existent tip
    let err = txsimulate::RPCTransactionSimulateRequestHandler::transaction_simulate(
        &tx,
        &StacksBlockId([0x01; 32]),
        &sort_db,
        chainstate,
        Duration::from_secs(30),
        Duration::from_secs(30),
        0,
    )
    .err()
    .unwrap();

    assert!(matches!(err, TxSimulateError::NoSuchTip));

    // an epoch-2.x tip exists, but cannot be extended by a Nakamoto block
    let err = txsimulate::RPCTransactionSimulateRequestHandler::transaction_simulate(
        &tx,
        &epoch2_tip,
        &sort_db,
        chainstate,
        Duration::from_secs(30),
        Duration::from_secs(30),
        0,
    )
    .err()
    .unwrap();

    let TxSimulateError::BadTip(reason) = err else {
        panic!("Expected BadTip for an epoch-2.x tip, got {err:?}");
    };
    assert_eq!(reason, "Chain tip is not a Nakamoto block");
}

/// Simulate a successful contract-call at the chain tip and check the
/// reported result, events, and execution cost.
#[test]
fn test_try_make_response() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);

    let test_observer = TestEventObserver::new();

    let private_key = StacksPrivateKey::from_seed("txsimulate".as_bytes());
    let address = to_addr(&private_key);

    let contract_name = ContractName::from_literal("test");
    let function_name = ClarityName::from_literal("test");

    // Set up the RPC test with a deployed contract to call
    let rpc_test =
        TestRPC::setup_nakamoto_with_boot_plan(function_name!(), &test_observer, |boot_plan| {
            let code_body =
        "(define-public (test) (stx-transfer? u100 tx-sender 'ST000000000000000000002AMW42H))";

            let contract_deploy = make_contract_publish_tx(
                &private_key,
                0,
                1000,
                CHAIN_ID_TESTNET,
                "test",
                code_body,
                None,
            );

            let boot_tenures = vec![NakamotoBootTenure::Sortition(vec![
                NakamotoBootStep::Block(vec![contract_deploy]),
            ])];

            boot_plan
                .with_boot_tenures(boot_tenures)
                .with_initial_balances(vec![(address.clone().into(), 1_000_000)])
        });

    let nakamoto_consensus_hash = rpc_test.consensus_hash.clone();
    let canonical_tip = rpc_test.canonical_tip.clone();

    let contract_call = make_contract_call_tx(
        &private_key,
        1,
        1000,
        CHAIN_ID_TESTNET,
        &address,
        contract_name.clone(),
        function_name.clone(),
        &[],
    );

    let mut requests = vec![];

    // simulate the contract-call at the tip
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::UseLatestAnchoredTip,
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    // a transaction with a stale nonce cannot be simulated
    let bad_nonce_call = make_contract_call_tx(
        &private_key,
        0,
        1000,
        CHAIN_ID_TESTNET,
        &address,
        contract_name.clone(),
        function_name.clone(),
        &[],
    );
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &bad_nonce_call,
        TipRequest::UseLatestAnchoredTip,
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    // unauthenticated request
    let request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::UseLatestAnchoredTip,
    );
    requests.push(request);

    // simulation is stateless: simulating the same transaction again should
    // still succeed with the same nonce
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::UseLatestAnchoredTip,
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    // simulate explicitly at the canonical tip via the `tip` query parameter
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::SpecificTip(canonical_tip.clone()),
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    // a non-existent `tip` yields a 404
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::SpecificTip(StacksBlockId([0x01; 32])),
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    // an epoch-2.x `tip` exists but cannot be extended, so it yields a 400
    // rather than a 500 -- the caller chose the tip, so it is a client error
    let epoch2_tip = last_epoch2_block_id(rpc_test.peer_1.chainstate_ref());
    let mut request = StacksHttpRequest::new_transaction_simulate(
        addr.into(),
        &contract_call,
        TipRequest::SpecificTip(epoch2_tip),
    );
    request.add_header("authorization".into(), "password".into());
    requests.push(request);

    let mut responses = rpc_test.run(requests);

    let tip_block = test_observer.get_blocks().last().unwrap().clone();

    // simulated the contract-call at the tip
    let response = responses.remove(0);

    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let resp = response.decode_simulated_transaction().unwrap();

    assert_eq!(resp.txid, contract_call.txid());
    assert_eq!(resp.tip_block_id, canonical_tip);
    assert_eq!(resp.consensus_hash, nakamoto_consensus_hash);
    assert_eq!(
        resp.block_height,
        tip_block.metadata.stacks_block_height + 1
    );

    assert_eq!(resp.result_hex, Value::okay_true());
    assert!(!resp.post_condition_aborted);
    assert!(resp.vm_error.is_none());

    // the mempool is empty, so there is nothing for the transaction to conflict with
    assert_eq!(resp.mempool_conflict, None);

    // the stx-transfer generated one event
    assert_eq!(resp.events.len(), 1);

    // a contract-call must have consumed some of the execution budget, and
    // the reported limit is the full tenure budget
    assert!(resp.execution_cost.runtime > 0);
    assert!(resp.execution_cost.exceeds(&ExecutionCost::ZERO));
    assert!(!resp.execution_cost.exceeds(&resp.execution_limit));

    // got a failure for the stale nonce (400)
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let (preamble, _body) = response.destruct();
    assert_eq!(preamble.status_code, 400);

    // got another failure (401 this time)
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let (preamble, _body) = response.destruct();
    assert_eq!(preamble.status_code, 401);

    // the repeated simulation succeeded with the same nonce, proving that
    // the earlier simulation did not mutate state
    let resp = responses.remove(0).decode_simulated_transaction().unwrap();
    assert_eq!(resp.result_hex, Value::okay_true());

    // simulating at an explicit `tip` equal to the canonical tip matches the
    // default behavior
    let resp = responses.remove(0).decode_simulated_transaction().unwrap();
    assert_eq!(resp.tip_block_id, canonical_tip);
    assert_eq!(resp.result_hex, Value::okay_true());

    // a non-existent `tip` yields a 404
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );
    let (preamble, _body) = response.destruct();
    assert_eq!(preamble.status_code, 404);

    // an epoch-2.x `tip` yields a 400, not a 500
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );
    let (preamble, contents) = response.destruct();
    assert_eq!(preamble.status_code, 400);
    let body: String = contents.try_into().unwrap();
    assert!(
        body.contains("Chain tip is not a Nakamoto block"),
        "Unexpected body: {body}"
    );
}

/// Set up a Nakamoto RPC test in which `private_key`'s account (balance 1,000,000 uSTX) has
/// deployed the contract `test`, with function `test`.  The next account nonce is 1.
fn setup_with_deployed_contract<'a>(
    test_name: &str,
    observer: &'a TestEventObserver,
    private_key: &StacksPrivateKey,
) -> TestRPC<'a> {
    let address = to_addr(private_key);
    TestRPC::setup_nakamoto_with_boot_plan(test_name, observer, |boot_plan| {
        let code_body =
            "(define-public (test) (stx-transfer? u100 tx-sender 'ST000000000000000000002AMW42H))";
        let contract_deploy = make_contract_publish_tx(
            private_key,
            0,
            1000,
            CHAIN_ID_TESTNET,
            "test",
            code_body,
            None,
        );
        let boot_tenures = vec![NakamotoBootTenure::Sortition(vec![
            NakamotoBootStep::Block(vec![contract_deploy]),
        ])];
        boot_plan
            .with_boot_tenures(boot_tenures)
            .with_initial_balances(vec![(address.clone().into(), 1_000_000)])
    })
}

/// Add `tx` to the serving peer's (`peer_2`) mempool, as if it had been submitted at the
/// canonical tip.
fn add_to_mempool(rpc_test: &mut TestRPC, tx: &StacksTransaction) {
    let tip_header = NakamotoChainState::get_block_header(
        rpc_test.peer_2.chainstate_ref().db(),
        &rpc_test.canonical_tip,
    )
    .unwrap()
    .unwrap();
    let origin_address = tx.origin_address();
    let origin_nonce = tx.get_origin_nonce();
    let sponsor_address = tx.sponsor_address().unwrap_or(origin_address.clone());
    let sponsor_nonce = tx.get_sponsor_nonce().unwrap_or(origin_nonce);

    let mut mempool = rpc_test.peer_2.mempool.take().unwrap();
    {
        let mut mempool_tx = mempool.tx_begin().unwrap();
        MemPoolDB::try_add_tx(
            &mut mempool_tx,
            rpc_test.peer_2.chainstate(),
            &tip_header.consensus_hash,
            &tip_header.anchored_header.block_hash(),
            true,
            &tx.txid(),
            tx.serialize_to_vec(),
            tx.get_tx_fee(),
            tip_header.stacks_block_height,
            &origin_address,
            origin_nonce,
            &sponsor_address,
            sponsor_nonce,
            None,
        )
        .unwrap();
        mempool_tx.commit().unwrap();
    }
    rpc_test.peer_2.mempool = Some(mempool);
}

/// A simulated transaction whose origin nonce is used by a transaction in the mempool is
/// reported in `mempool_conflict`, with the outcome the mempool would give it at submission.
/// The simulation itself still succeeds with a 200, and the mempool is left unchanged.
///
/// The mempool transaction P is a contract-call with nonce 1 and fee 1000.  All candidates use
/// nonce 1 too, so they conflict with P, and are all in P's tenure (the canonical tip's): a
/// higher fee replaces P, anything else is rejected, and P itself is recognized as already in
/// the mempool.  (A candidate with a free nonce cannot be simulated while P is pending, and the
/// no-conflict case is covered by `test_try_make_response`.)
#[test]
fn test_transaction_simulate_mempool_conflict() {
    let test_observer = TestEventObserver::new();
    let private_key = StacksPrivateKey::from_seed("txsimulate".as_bytes());
    let address = to_addr(&private_key);
    let mut rpc_test = setup_with_deployed_contract(function_name!(), &test_observer, &private_key);

    let call = |fee: u64| {
        make_contract_call_tx(
            &private_key,
            1,
            fee,
            CHAIN_ID_TESTNET,
            &address,
            ContractName::from_literal("test"),
            ClarityName::from_literal("test"),
            &[],
        )
    };

    let prior_tx = call(1000);
    add_to_mempool(&mut rpc_test, &prior_tx);

    // same nonce and same fee as P, but a different transaction (a transfer to another account;
    // self-transfers are invalid)
    let same_fee_tx = make_stacks_transfer_tx(
        &private_key,
        1,
        1000,
        CHAIN_ID_TESTNET,
        &PrincipalData::from(to_addr(&StacksPrivateKey::from_seed(
            "recipient".as_bytes(),
        ))),
        1,
    );
    assert_ne!(same_fee_tx.txid(), prior_tx.txid());
    let candidates = vec![
        prior_tx.clone(),
        same_fee_tx,
        call(999),
        call(1001),
        // simulating again after the above shows that simulation left the mempool unchanged
        prior_tx.clone(),
    ];
    let requests = candidates
        .iter()
        .map(|tx| {
            let mut request = StacksHttpRequest::new_transaction_simulate(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333).into(),
                tx,
                TipRequest::UseLatestAnchoredTip,
            );
            request.add_header("authorization".into(), "password".into());
            request
        })
        .collect();

    // before every request (including the last), the mempool must hold only P
    let prior_txid = prior_tx.txid();
    let responses = rpc_test.run_with_observer(requests, None, |_, peer_2| {
        let mempool = peer_2.mempool.as_ref().unwrap();
        let txs = MemPoolDB::get_all_txs(mempool.conn()).unwrap();
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].tx.txid(), prior_txid);
        true
    });

    let conflict = |outcome| {
        Some(RPCMempoolConflict {
            txid: prior_tx.txid(),
            tx_fee: 1000,
            outcome,
        })
    };
    let expected = vec![
        conflict(RPCMempoolConflictOutcome::AlreadyInMempool),
        conflict(RPCMempoolConflictOutcome::Rejected),
        conflict(RPCMempoolConflictOutcome::Rejected),
        conflict(RPCMempoolConflictOutcome::ReplaceByFee),
        conflict(RPCMempoolConflictOutcome::AlreadyInMempool),
    ];
    assert_eq!(responses.len(), expected.len());

    for ((response, expected), tx) in responses.into_iter().zip(expected).zip(&candidates) {
        let resp = response
            .decode_simulated_transaction()
            .unwrap_or_else(|e| panic!("simulating tx with fee {} failed: {e:?}", tx.get_tx_fee()));
        // the conflict never prevents the simulation itself from succeeding
        assert_eq!(resp.txid, tx.txid());
        assert_eq!(resp.result_hex, Value::okay_true());
        assert_eq!(
            resp.mempool_conflict,
            expected,
            "tx fee {}",
            tx.get_tx_fee()
        );
    }
}

/// If the mempool cannot be queried, the simulation request fails with a 500 rather than
/// reporting a misleading "no conflict".  The chain tip and the transaction are valid, so
/// without the mempool failure, the simulation would succeed.
#[test]
fn test_transaction_simulate_mempool_error() {
    let test_observer = TestEventObserver::new();
    let private_key = StacksPrivateKey::from_seed("txsimulate".as_bytes());
    let address = to_addr(&private_key);
    let mut rpc_test = setup_with_deployed_contract(function_name!(), &test_observer, &private_key);

    // break the serving peer's mempool
    rpc_test
        .peer_2
        .mempool
        .as_ref()
        .unwrap()
        .conn()
        .execute("DROP TABLE mempool", [])
        .unwrap();

    let contract_call = make_contract_call_tx(
        &private_key,
        1,
        1000,
        CHAIN_ID_TESTNET,
        &address,
        ContractName::from_literal("test"),
        ClarityName::from_literal("test"),
        &[],
    );

    // checking the mempool directly reports a `MemPool` error
    let canonical_tip = rpc_test.canonical_tip.clone();
    let mempool = rpc_test.peer_2.mempool.take().unwrap();
    let err = txsimulate::RPCTransactionSimulateRequestHandler::check_mempool_conflict(
        &contract_call,
        &canonical_tip,
        rpc_test.peer_2.chainstate(),
        mempool.conn(),
    )
    .unwrap_err();
    assert!(matches!(err, TxSimulateError::MemPool(_)), "got {err:?}");
    rpc_test.peer_2.mempool = Some(mempool);

    // and so does the endpoint
    let mut request = StacksHttpRequest::new_transaction_simulate(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333).into(),
        &contract_call,
        TipRequest::UseLatestAnchoredTip,
    );
    request.add_header("authorization".into(), "password".into());

    let response = rpc_test.run_one(request);
    let (preamble, contents) = response.destruct();
    assert_eq!(preamble.status_code, 500);
    let body: String = contents.try_into().unwrap();
    assert!(
        body.contains("Failed to check mempool for conflicts"),
        "Unexpected body: {body}"
    );
}
