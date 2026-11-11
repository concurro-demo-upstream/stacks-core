// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use clarity::types::chainstate::StacksBlockId;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::ClarityName;
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::types::Address;

use super::{assert_squashed_proof_error, test_rpc, TestRPC};
use crate::net::api::*;
use crate::net::connection::ConnectionOptions;
use crate::net::http::HttpRequestContents;
use crate::net::httpcore::{
    HttpRequestContentsExtensions as _, RPCRequestHandler, StacksHttp, StacksHttpRequest,
};
use crate::net::{ProtocolFamily, TipRequest};

#[test]
fn test_try_parse_request() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut http = StacksHttp::new(addr, &ConnectionOptions::default());

    let request = StacksHttpRequest::new_getdatavar(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world-unconfirmed".try_into().unwrap(),
        "test-var".try_into().unwrap(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32])),
        true,
    );
    assert_eq!(
        request.contents().tip_request(),
        TipRequest::SpecificTip(StacksBlockId([0x22; 32]))
    );
    assert!(request.contents().get_with_proof());

    let bytes = request.try_serialize().unwrap();

    debug!("Request:\n{}\n", std::str::from_utf8(&bytes).unwrap());

    let (parsed_preamble, offset) = http.read_preamble(&bytes).unwrap();
    let mut handler = getdatavar::RPCGetDataVarRequestHandler::new();
    let mut parsed_request = http
        .handle_try_parse_request(
            &mut handler,
            &parsed_preamble.expect_request(),
            &bytes[offset..],
        )
        .unwrap();

    // parsed request consumes headers that would not be in a constructed reqeuest
    parsed_request.clear_headers();
    let (preamble, contents) = parsed_request.destruct();

    // consumed path args
    assert_eq!(
        handler.contract_identifier,
        Some(
            QualifiedContractIdentifier::parse(
                "ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R.hello-world-unconfirmed"
            )
            .unwrap()
        )
    );
    assert_eq!(handler.varname, Some(ClarityName::from_literal("test-var")));

    assert_eq!(&preamble, request.preamble());

    handler.restart();
    assert!(handler.contract_identifier.is_none());
    assert!(handler.varname.is_none());
}

#[test]
fn test_try_make_response() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);

    let mut requests = vec![];

    // query existing
    let request = StacksHttpRequest::new_getdatavar(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "bar".try_into().unwrap(),
        TipRequest::UseLatestAnchoredTip,
        true,
    );
    requests.push(request);

    // query existing unconfirmed
    let request = StacksHttpRequest::new_getdatavar(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world-unconfirmed".try_into().unwrap(),
        "bar-unconfirmed".try_into().unwrap(),
        TipRequest::UseLatestUnconfirmedTip,
        true,
    );
    requests.push(request);

    // query non-existant var
    let request = StacksHttpRequest::new_getdatavar(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "hello-world".try_into().unwrap(),
        "does-not-exist".try_into().unwrap(),
        TipRequest::UseLatestAnchoredTip,
        true,
    );
    requests.push(request);

    // query non-existant contract
    let request = StacksHttpRequest::new_getdatavar(
        addr.into(),
        StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
        "does-not-exist".try_into().unwrap(),
        "bar".try_into().unwrap(),
        TipRequest::UseLatestAnchoredTip,
        true,
    );
    requests.push(request);

    let mut responses = test_rpc(function_name!(), requests);

    // latest data
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let resp = response.decode_data_var_response().unwrap();
    assert_eq!(resp.data, "0x0000000000000000000000000000000000");
    assert!(resp.marf_proof.is_some());

    // unconfirmed data
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let resp = response.decode_data_var_response().unwrap();
    assert_eq!(resp.data, "0x0100000000000000000000000000000001");
    assert!(resp.marf_proof.is_some());

    // no such var
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let (preamble, body) = response.destruct();
    assert_eq!(preamble.status_code, 404);

    // no such contract
    let response = responses.remove(0);
    debug!(
        "Response:\n{}\n",
        std::str::from_utf8(&response.try_serialize().unwrap()).unwrap()
    );

    let (preamble, body) = response.destruct();
    assert_eq!(preamble.status_code, 404);
}

/// Proof reads on a squashed Clarity MARF must fail with an explicit error instead of a
/// misleading 404. Reads without proofs keep working on squashed state.
#[test]
fn test_try_make_response_squashed() {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 33333);
    let mut test = TestRPC::setup(function_name!());
    test.squash_serving_clarity_state();
    let tip = TipRequest::SpecificTip(test.canonical_tip.clone());

    let data_var = |name: &str, with_proof: bool| {
        StacksHttpRequest::new_getdatavar(
            addr.into(),
            StacksAddress::from_string("ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R").unwrap(),
            "hello-world".try_into().unwrap(),
            name.try_into().unwrap(),
            tip.clone(),
            with_proof,
        )
    };

    let requests = vec![
        // default proof (proofs are returned unless the client opts out)
        StacksHttpRequest::new_for_peer(
            addr.into(),
            "GET".into(),
            "/v2/data_var/ST2DS4MSWSGJ3W9FBC6BVT0Y92S345HY8N3T6AV7R/hello-world/bar".into(),
            HttpRequestContents::new().for_tip(tip.clone()),
        )
        .unwrap(),
        // explicit proof=1, existing and missing vars
        data_var("bar", true),
        data_var("does-not-exist", true),
        // proof=0 still reads the stored value, and 404s for a missing var
        data_var("bar", false),
        data_var("does-not-exist", false),
    ];
    let mut responses = test.run(requests);

    for _ in 0..3 {
        assert_squashed_proof_error(responses.remove(0));
    }

    let resp = responses.remove(0).decode_data_var_response().unwrap();
    assert_eq!(resp.data, "0x0000000000000000000000000000000000");
    assert!(resp.marf_proof.is_none());

    let (preamble, _) = responses.remove(0).destruct();
    assert_eq!(preamble.status_code, 404);
}
