// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Persisted Ruby facts must preserve uncertainty through real extraction and linking.

use super::rspec::{SPEC, calls, program};
use super::{assert_unresolved, index, labelled, resolved};
use mvp_model::EdgeKind::Call;

#[test]
fn implicit_subject_should_not_type_an_undefined_memoized_method_as_the_described_class() {
    let rows = program(
        "RSpec.describe Purchase do; it('works') { missing.refund! }; end",
        &[],
    );
    assert_eq!(calls(&rows), Default::default());
    assert_unresolved(&rows, &[(SPEC, Call, 1, "missing.refund!")]);
}

#[test]
fn a_def_after_a_same_line_let_should_not_inherit_the_lets_typed_return_value() {
    let rows = program(
        "RSpec.describe Purchase do; let(:purchase) { Purchase.new }; def purchase; end; it('works') { purchase.refund! }; end",
        &[],
    );
    assert_eq!(calls(&rows), labelled(&[(SPEC, Call, 1, "purchase")]));
    assert_unresolved(&rows, &[(SPEC, Call, 1, "purchase.refund!")]);
}

#[test]
fn a_long_finite_let_chain_should_hit_its_budget_without_resolving_a_later_receiver() {
    let mut source = "RSpec.describe Purchase do; let(:v0) { Purchase.new };".to_string();
    for n in 1..12 {
        source.push_str(&format!("let(:v{n}) {{ v{} }};", n - 1));
    }
    source.push_str("it('works') { v11.refund! }; end");
    let rows = program(&source, &[]);
    assert_eq!(calls(&rows), Default::default());
    assert_unresolved(&rows, &[(SPEC, Call, 1, "v11.refund!")]);
}

#[test]
fn nullable_association_readers_should_not_resolve_nil_methods_to_the_model() {
    let rows = index(&[
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base; end",
        ),
        (
            "app/models/room.rb",
            "class Room < ApplicationRecord; belongs_to :user; end",
        ),
        (
            "app/models/user.rb",
            "class User < ApplicationRecord; def label; end; def to_s; end; end",
        ),
        (
            "app/jobs/check.rb",
            "class Check; def run; Room.find(1).user.label; Room.find(1).user.to_s; end; end",
        ),
    ]);
    let calls = resolved(&rows)
        .into_iter()
        .filter(|(src, kind, _, _)| src == "Check#run" && *kind == Call)
        .map(|(_, _, _, dst)| dst)
        .collect::<Vec<_>>();
    assert_eq!(calls, ["User#label"]);
    assert_unresolved(&rows, &[("Check#run", Call, 1, "Room.find(1).user.to_s")]);
}
