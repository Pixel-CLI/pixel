// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Receivers typed by Rails conventions (ADR-0038): a chain rooted at a
//! model's finder, an association reader, `reload` and a relation's scope
//! reach what Rails dispatches them to; an override, a redeclaration, an
//! extension, a value that may be `nil` or an Array, or a class that is no
//! model leaves them unresolved.

use std::collections::BTreeSet;

use mvp_model::EdgeKind::Call;
use mvp_model::FileRows;

use super::{assert_unresolved, index, resolved};

/// The models every test starts from: `Room` has many `messages` and a
/// `scope :recent`; `Message` belongs to its `room`.
const MODELS: &[(&str, &str)] = &[
    (
        "app/models/application_record.rb",
        "class ApplicationRecord < ActiveRecord::Base
  self.abstract_class = true
end
",
    ),
    (
        "app/models/room.rb",
        "class Room < ApplicationRecord
  has_many :messages
  def label
  end
  def to_s
  end
  def size
  end
end
",
    ),
    (
        "app/models/message.rb",
        "class Message < ApplicationRecord
  belongs_to :room
  scope :recent, -> { order(:id) }
  def self.latest
  end
  def deliver
  end
end
",
    ),
];

/// The rows of the models with `extra` added.
fn with(extra: &[(&'static str, &'static str)]) -> Vec<FileRows> {
    let mut files = MODELS.to_vec();
    files.extend_from_slice(extra);
    index(&files)
}

/// The resolved calls of `caller` in `rows`: (line, target).
fn calls_of(rows: &[FileRows], caller: &str) -> BTreeSet<(u32, String)> {
    resolved(rows)
        .into_iter()
        .filter(|(src, kind, _, _)| src == caller && *kind == Call)
        .map(|(_, _, line, dst)| (line, dst))
        .collect()
}

fn set(rows: &[(u32, &str)]) -> BTreeSet<(u32, String)> {
    rows.iter().map(|(l, d)| (*l, (*d).to_owned())).collect()
}

/// A finder of a model, an association reader of the record it returns, a
/// record a relation builds, `reload` and a scope on a relation each
/// reach what Rails runs: `Room.find(id)` is a `Room`, its `messages` a
/// relation of `Message`, whose `create!` is a `Message` and `recent` its
/// scope; `Message.find_by(..).room` a `Room` or `nil`.
#[test]
fn c10_ruby_typed_a_model_finder_and_its_associations_reach_the_record_class() {
    let rows = with(&[(
        "app/jobs/sweep.rb",
        "class Sweep
  def run(id)
    Room.find(id).label
    Room.find(id).messages.create!(body: 1).deliver
    Room.find(id).messages.recent
    Message.find_by(id: id).room.label
    Room.where(open: true).first.label
    Room.find(id).reload.label
    Room.find(id).messages.where(x: 1).last!.deliver
  end
end
",
    )]);
    assert_eq!(
        calls_of(&rows, "Sweep#run"),
        set(&[
            (3, "Room#label"),
            (4, "Message#deliver"),
            (5, "Message.recent"),
            (6, "Room#label"),
            (7, "Room#label"),
            (8, "Room#label"),
            (9, "Message#deliver"),
        ])
    );
}

/// What may not be a record of the class proves nothing: a class that is
/// no model (`Plain.find`, `Remote < ActiveResource::Base`), a model whose
/// own `find` runs instead, a
/// value that may be `nil` (`find_by`) or an Array (`find(ids)`) for a
/// method `nil` or an Array has, `first(2)` (an Array), a class method
/// that is no scope on a relation (the relation's own methods come
/// first), and the reader `room` itself, which Rails generates.
#[test]
fn c10_ruby_typed_what_may_be_another_object_abstains() {
    let rows = with(&[
        (
            "lib/plain.rb",
            "class Plain
  def run
  end
end
class Remote < ActiveResource::Base
  def run
  end
end
",
        ),
        (
            "app/models/report.rb",
            "class Report < ApplicationRecord
  def self.find(id)
  end
  def label
  end
end
",
        ),
        (
            "app/jobs/sweep.rb",
            "class Sweep
  def run(id, ids)
    Plain.find(id).run
    Report.find(id).label
    Room.find_by(id: id).to_s
    Room.find(ids).size
    Room.first(2).label
    Room.find(id).messages.latest
    Message.find(id).room
    Room.find(id).label
    Room.find(id, id).label
    Room.where(open: true).find(id) { |r| r }.label
    Remote.find(id).run
  end
end
",
        ),
    ]);
    assert_eq!(
        calls_of(&rows, "Sweep#run"),
        set(&[(4, "Report.find"), (10, "Room#label")])
    );
    assert_unresolved(
        &rows,
        &[
            ("Sweep#run", Call, 3, "Plain.find(id).run"),
            ("Sweep#run", Call, 4, "Report.find(id).label"),
            ("Sweep#run", Call, 5, "Room.find_by(id: id).to_s"),
            ("Sweep#run", Call, 6, "Room.find(ids).size"),
            ("Sweep#run", Call, 7, "Room.first(2).label"),
            ("Sweep#run", Call, 8, "Room.find(id).messages.latest"),
            ("Sweep#run", Call, 9, "Message.find(id).room"),
            ("Sweep#run", Call, 11, "Room.find(id, id).label"),
            (
                "Sweep#run",
                Call,
                12,
                "Room.where(open: true).find(id) { |r| r }.label",
            ),
            ("Sweep#run", Call, 13, "Remote.find(id).run"),
        ],
    );
}

/// An association reader the code may change gives no type: a method of
/// the same name overrides it, a subclass that may be the record
/// redeclares it with another class, an extension's methods come before
/// the relation's, a `through:` association names its class elsewhere,
/// and one declared in a concern's `included do` lands where the rules do
/// not place it.
#[test]
fn c10_ruby_typed_an_association_the_code_may_change_abstains() {
    let rows = index(&[
        MODELS[0],
        MODELS[2],
        (
            "app/models/archive.rb",
            "class Archive < ApplicationRecord
  def deliver
  end
end
",
        ),
        (
            "app/models/concerns/tagged.rb",
            "module Tagged
  extend ActiveSupport::Concern
  included do
    has_many :tags
  end
end
",
        ),
        (
            "app/models/tag.rb",
            "class Tag < ApplicationRecord
  def deliver
  end
end
",
        ),
        (
            "app/models/room.rb",
            "class Room < ApplicationRecord
  include Tagged
  has_many :messages
  has_many :posts, class_name: \"Message\" do
    def recent
    end
  end
  has_many :notes, class_name: \"Message\"
  has_many :tags, class_name: \"Message\"
  has_one :pin, through: :notes, class_name: \"Message\"
  def notes
  end
end
",
        ),
        (
            "app/models/rooms/closed.rb",
            "module Rooms
  class Closed < Room
    has_many :messages, class_name: \"Archive\"
  end
end
",
        ),
        (
            "app/jobs/sweep.rb",
            "class Sweep
  def run(id)
    Room.find(id).messages.first.deliver
    Room.find(id).posts.recent
    Room.find(id).posts.first.deliver
    Room.find(id).notes.first.deliver
    Room.find(id).tags.first.deliver
    Room.find(id).pin.deliver
    Rooms::Closed.find(id).messages.first.deliver
  end
end
",
        ),
    ]);
    assert_eq!(calls_of(&rows, "Sweep#run"), set(&[(9, "Archive#deliver")]));
    assert_unresolved(
        &rows,
        &[
            ("Sweep#run", Call, 3, "Room.find(id).messages.first.deliver"),
            ("Sweep#run", Call, 4, "Room.find(id).posts.recent"),
            ("Sweep#run", Call, 5, "Room.find(id).posts.first.deliver"),
            ("Sweep#run", Call, 6, "Room.find(id).notes.first.deliver"),
            ("Sweep#run", Call, 7, "Room.find(id).tags.first.deliver"),
            ("Sweep#run", Call, 8, "Room.find(id).pin.deliver"),
        ],
    );
}

/// A record whose class has subclasses reaches a method only when every
/// subclass finds the same one (single-table inheritance makes `find`
/// return any of them); an association named like a superclass's method
/// shadows it, so a call of that name reaches nothing; and `reload` a
/// subclass overrides may return anything.
#[test]
fn c10_ruby_typed_a_record_may_be_any_subclass_and_a_reader_shadows_a_method() {
    let rows = index(&[
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base
  def owner
  end
end
",
        ),
        (
            "app/models/user.rb",
            "class User < ApplicationRecord
end
",
        ),
        (
            "app/models/room.rb",
            "class Room < ApplicationRecord
  belongs_to :owner, class_name: \"User\"
  def label
  end
  def close
  end
end
",
        ),
        (
            "app/models/rooms/direct.rb",
            "module Rooms
  class Direct < Room
    def label
    end
    def reload
    end
  end
end
",
        ),
        (
            "app/jobs/sweep.rb",
            "class Sweep
  def run(id)
    Room.find(id).label
    Room.find(id).close
    Room.find(id).owner
    Room.new.label
    Room.find(id).reload.close
  end
end
",
        ),
    ]);
    assert_eq!(calls_of(&rows, "Sweep#run"), set(&[(4, "Room#close")]));
    assert_unresolved(
        &rows,
        &[
            ("Sweep#run", Call, 3, "Room.find(id).label"),
            ("Sweep#run", Call, 5, "Room.find(id).owner"),
            ("Sweep#run", Call, 6, "Room.new.label"),
            ("Sweep#run", Call, 7, "Room.find(id).reload.close"),
        ],
    );
}

/// A local bound once, by a plain assignment parsed before the read, holds
/// what its value holds, also in a block that closes over it; it may be
/// `nil` where the assignment did not run, so `nil`'s methods abstain.
#[test]
fn c10_ruby_typed_a_local_bound_once_holds_its_value() {
    let rows = with(&[
        (
            "lib/widget.rb",
            "class Widget
  def run
  end
end
",
        ),
        (
            "app/jobs/sweep.rb",
            "class Sweep
  def run(id)
    room = Room.find_by!(id: id)
    room.label
    messages = room.messages
    messages.recent
    message = messages.create!(body: 1)
    message.deliver
    [1].each { room.label }
    widget = Widget.new
    widget.run
    room.to_s
    Room.find_by!(id: id).to_s
  end
end
",
        ),
    ]);
    assert_eq!(
        calls_of(&rows, "Sweep#run"),
        set(&[
            (4, "Room#label"),
            (6, "Message.recent"),
            (8, "Message#deliver"),
            (9, "Room#label"),
            (11, "Widget#run"),
            (13, "Room#to_s"),
        ])
    );
    assert_unresolved(&rows, &[("Sweep#run", Call, 12, "room.to_s")]);
}

/// A local the code may bind again, or another way, holds nothing the
/// rules can type: assigned twice, a parameter of the same name, a block
/// parameter shadowing it, `||=`, a `rescue` variable, a read the parser
/// takes for a method call (before the assignment, or past the block
/// that bound it), a gate that
/// evaluates code (`eval`) or hands its binding out, and a value the
/// read itself is part of, where the local is still `nil`.
#[test]
fn c10_ruby_typed_a_local_bound_more_than_once_abstains() {
    let rows = with(&[(
        "app/jobs/sweep.rb",
        "class Sweep
  def twice(id)
    room = Room.find(id)
    room = Room.find(id + 1) if id
    room.label
  end
  def param(room)
    room = Room.find(1)
    room.label
  end
  def shadowed(rooms)
    room = Room.find(1)
    rooms.each { |room| room.label }
    room.label
  end
  def memo(id)
    room ||= Room.find(id)
    room.label
  end
  def rescued
    room = Room.find(1)
  rescue => room
    room.label
  end
  def early(id)
    room.label
    room = Room.find(id)
  end
  def evaluated(id)
    room = Room.find(id)
    eval(\"room = 1\")
    room.label
  end
  def itself(id)
    room = room.messages.first
    room.label
  end
  def closed
    [1].each { room = Room.find(1) }
    room.label
  end
  def inside
    room = Room.find_by(id: room.label)
  end
  def bound(id)
    room = Room.find(id)
    helper(binding)
    room.label
  end
end
",
    )]);
    assert_eq!(
        calls_of(&rows, "Sweep#twice")
            .into_iter()
            .chain(calls_of(&rows, "Sweep#param"))
            .chain(calls_of(&rows, "Sweep#shadowed"))
            .chain(calls_of(&rows, "Sweep#memo"))
            .chain(calls_of(&rows, "Sweep#rescued"))
            .chain(calls_of(&rows, "Sweep#early"))
            .chain(calls_of(&rows, "Sweep#evaluated"))
            .chain(calls_of(&rows, "Sweep#itself"))
            .chain(calls_of(&rows, "Sweep#closed"))
            .chain(calls_of(&rows, "Sweep#inside"))
            .chain(calls_of(&rows, "Sweep#bound"))
            .collect::<BTreeSet<_>>(),
        set(&[])
    );
    assert_unresolved(
        &rows,
        &[
            ("Sweep#twice", Call, 5, "room.label"),
            ("Sweep#param", Call, 9, "room.label"),
            ("Sweep#shadowed", Call, 13, "room.label"),
            ("Sweep#shadowed", Call, 14, "room.label"),
            ("Sweep#memo", Call, 18, "room.label"),
            ("Sweep#rescued", Call, 23, "room.label"),
            ("Sweep#early", Call, 26, "room.label"),
            ("Sweep#evaluated", Call, 32, "room.label"),
            ("Sweep#itself", Call, 36, "room.label"),
            ("Sweep#closed", Call, 40, "room.label"),
            ("Sweep#inside", Call, 43, "room.label"),
            ("Sweep#bound", Call, 48, "room.label"),
        ],
    );
}

/// The test helper and fixture files of a Minitest suite: `fixtures
/// :all`, a set per model, one that names its class, and a helper
/// method named like a set.
const SUITE: &[(&str, &str)] = &[
    (
        "test/test_helper.rb",
        "class ActiveSupport::TestCase
  fixtures :all
  def messages(*names)
  end
end
",
    ),
    ("test/fixtures/rooms.yml", "pets:\n  name: Pets\n"),
    ("test/fixtures/messages.yml", "first:\n  body: Hi\n"),
    (
        "test/fixtures/archives.yml",
        "_fixture:\n  model_class: Room\nold:\n  name: Old\n",
    ),
    ("test/fixtures/headers.yml", "plain:\n  name: Plain\n"),
    ("test/fixtures/widgets.yml", "one:\n  name: One\n"),
    (
        "app/models/archive.rb",
        "class Archive < ApplicationRecord\n  def label\n  end\nend\n",
    ),
    (
        "app/models/header.rb",
        "class Header < ApplicationRecord\n  def label\n  end\nend\n",
    ),
    ("lib/widget.rb", "class Widget\n  def label\n  end\nend\n"),
    ("test/fixtures/notes.yml", "one:\n  body: One\n"),
    (
        "app/models/note.rb",
        "class Note < ApplicationRecord\n  def label\n  end\nend\n",
    ),
    // Its superclass may be the test case: its own methods come first.
    (
        "test/support/sneaky.rb",
        "class Sneaky < (ENV[\"X\"] ? RoomTest : Object)\n  def notes(name)\n  end\nend\n",
    ),
];

/// A fixture accessor in a Minitest test, a `setup` or a test method, in
/// blocks that keep `self`, returns a record of the model its file names
/// (`test/fixtures/rooms.yml` → `Room`), held by a local too; a method of
/// the same name on a class the test case's chain does not hold
/// (`Load#rooms`), or by one whose superclass may be the test case but
/// which does not define it (`Sneaky`), does not answer it.
#[test]
fn c10_ruby_typed_a_fixture_accessor_returns_its_model() {
    let mut files = MODELS.to_vec();
    files.extend_from_slice(SUITE);
    files.push(("lib/load.rb", "class Load\n  def rooms\n  end\nend\n"));
    files.push((
        "test/models/room_test.rb",
        "class RoomTest < ActiveSupport::TestCase
  setup do
    rooms(:pets).label
  end
  test \"labels\" do
    room = rooms(:pets)
    room.messages.recent
    [1].each { rooms(:pets).label }
  end
  def helper
    rooms(\"pets\").label
  end
end
",
    ));
    let rows = index(&files);
    assert_eq!(
        calls_of(&rows, "RoomTest"),
        set(&[(3, "Room#label"), (7, "Message.recent"), (8, "Room#label")])
    );
    assert_eq!(
        calls_of(&rows, "RoomTest#helper"),
        set(&[(11, "Room#label")])
    );
}

/// A fixture accessor proves nothing where something else may answer it:
/// a method of that name in the program (`messages`), a set that names
/// its class (`archives`), one named as a method of Rails' test classes
/// (`headers`), one whose class is no model (`widgets`), one an object
/// whose superclass may be the test case defines (`notes`), two names (an
/// Array), `self` not a test case
/// (the class body, a block given to `instance_exec`), and a file outside
/// the Minitest suite.
#[test]
fn c10_ruby_typed_a_fixture_accessor_something_else_may_answer_abstains() {
    let mut files = MODELS.to_vec();
    files.extend_from_slice(SUITE);
    files.push((
        "test/models/room_test.rb",
        "class RoomTest < ActiveSupport::TestCase
  rooms(:pets).label
  test \"labels\" do
    messages(:first).deliver
    archives(:old).label
    rooms(:pets, :pets).label
    instance_exec { rooms(:pets).label }
    headers(:plain).label
    widgets(:one).label
    notes(:one).label
  end
end
",
    ));
    files.push((
        "spec/models/room_spec.rb",
        "class RoomSpec < ActiveSupport::TestCase
  test \"labels\" do
    rooms(:pets).label
  end
end
",
    ));
    // A subclass's own method answers on its instances, which run the
    // test case's blocks too.
    files.push((
        "test/models/booth_test.rb",
        "class BoothTest < ActiveSupport::TestCase
  test \"labels\" do
    rooms(:pets).label
  end
end
class BoothSubTest < BoothTest
  def rooms(name)
  end
end
",
    ));
    let rows = index(&files);
    assert_eq!(calls_of(&rows, "RoomTest"), set(&[]));
    assert_eq!(calls_of(&rows, "RoomSpec"), set(&[]));
    assert_eq!(calls_of(&rows, "BoothTest"), set(&[]));
    assert_unresolved(
        &rows,
        &[
            ("RoomTest", Call, 2, "rooms(:pets).label"),
            ("RoomTest", Call, 4, "messages(:first).deliver"),
            ("RoomTest", Call, 5, "archives(:old).label"),
            ("RoomTest", Call, 6, "rooms(:pets, :pets).label"),
            ("RoomTest", Call, 7, "rooms(:pets).label"),
            ("RoomTest", Call, 8, "headers(:plain).label"),
            ("RoomTest", Call, 9, "widgets(:one).label"),
            ("RoomTest", Call, 10, "notes(:one).label"),
            ("RoomSpec", Call, 3, "rooms(:pets).label"),
            ("BoothTest", Call, 3, "rooms(:pets).label"),
        ],
    );
}

/// Without `fixtures` declared, or with a set's class renamed
/// (`set_fixture_class`), no accessor is typed.
#[test]
fn c10_ruby_typed_fixtures_undeclared_or_renamed_type_nothing() {
    let test = (
        "test/models/room_test.rb",
        "class RoomTest < ActiveSupport::TestCase
  test \"labels\" do
    rooms(:pets).label
  end
end
",
    );
    for helper in [
        "class ActiveSupport::TestCase\nend\n",
        "class ActiveSupport::TestCase\n  fixtures :all\n  set_fixture_class rooms: Room\nend\n",
    ] {
        let mut files = MODELS.to_vec();
        files.push(("test/test_helper.rb", helper));
        files.push(("test/fixtures/rooms.yml", "pets:\n  name: Pets\n"));
        files.push(test);
        let rows = index(&files);
        assert_unresolved(&rows, &[("RoomTest", Call, 3, "rooms(:pets).label")]);
    }
}
