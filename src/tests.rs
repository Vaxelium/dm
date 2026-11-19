use common::{DmReply, DmRequest};

use crate::db::Db;
use crate::service::{apply, Identity};

fn who(id: &str, name: &str) -> Identity {
    Identity { id: id.into(), name: name.into() }
}

fn alice() -> Identity {
    who("alice", "Alice")
}

fn bob() -> Identity {
    who("bob", "Bob")
}

fn carol() -> Identity {
    who("carol", "Carol")
}

/// Sends one request as `me`, looking up the other person's name the way the network layer does: `names`
/// holds the accounts that exist, by id.
fn ask(db: &Db, me: &Identity, request: DmRequest) -> DmReply {
    let names = [("alice", "Alice"), ("bob", "Bob"), ("carol", "Carol")];
    let other = match &request {
        DmRequest::Open { with, .. } => names.iter().find(|(id, _)| id == with).map(|(_, n)| n.to_string()),
        DmRequest::Send { to, .. } => names.iter().find(|(id, _)| id == to).map(|(_, n)| n.to_string()),
        _ => None,
    };
    apply(db, me, request, other)
}

fn open(db: &Db, me: &Identity, with: &str) -> i64 {
    match ask(db, me, DmRequest::Open { token: "t".into(), with: with.into() }) {
        DmReply::Opened { conversation } => conversation.channel_id,
        other => panic!("open failed: {other:?}"),
    }
}

fn send(db: &Db, me: &Identity, to: &str, text: &str, reply_to: Option<i64>) -> common::ChatMessage {
    let request = DmRequest::Send { token: "t".into(), to: to.into(), text: text.into(), reply_to };
    match ask(db, me, request) {
        DmReply::Changed { message } => message,
        other => panic!("send failed: {other:?}"),
    }
}

fn error(reply: DmReply) -> String {
    match reply {
        DmReply::Error { text } => text,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_conversation_is_made_once_and_shows_the_other_person() {
    let db = Db::open(":memory:").unwrap();
    let first = open(&db, &alice(), "bob");
    let again = open(&db, &bob(), "alice");
    assert_eq!(first, again, "both people share one conversation");
    match ask(&db, &bob(), DmRequest::List { token: "t".into() }) {
        DmReply::Conversations { list, .. } => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].name, "Alice");
            assert_eq!(list[0].other_id, "alice");
            assert_eq!(list[0].channel, format!("dm-{first}"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn messages_are_kept_with_their_author_and_show_in_history() {
    let db = Db::open(":memory:").unwrap();
    let id = open(&db, &alice(), "bob");
    let hello = send(&db, &alice(), "bob", "  hello  ", None);
    assert_eq!(hello.text, "hello", "surrounding spaces are trimmed");
    assert_eq!(hello.author, "Alice");
    assert_eq!(hello.author_id, "alice");
    assert_eq!(hello.channel_id, id);
    match ask(&db, &bob(), DmRequest::History { token: "t".into(), channel_id: id }) {
        DmReply::Messages { messages } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text, "hello");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_the_two_people_see_a_conversation() {
    let db = Db::open(":memory:").unwrap();
    let id = open(&db, &alice(), "bob");
    let message = send(&db, &alice(), "bob", "private", None);
    let refused = error(ask(&db, &carol(), DmRequest::History { token: "t".into(), channel_id: id }));
    assert_eq!(refused, "that conversation is not yours");
    let refused = error(ask(&db, &carol(), DmRequest::React { token: "t".into(), id: message.id, emoji: "👍".into() }));
    assert_eq!(refused, "that conversation is not yours");
    let refused = error(ask(&db, &carol(), DmRequest::Edit { token: "t".into(), id: message.id, text: "x".into() }));
    assert_eq!(refused, "that conversation is not yours");
}

#[test]
fn you_cannot_message_yourself_or_a_missing_account() {
    let db = Db::open(":memory:").unwrap();
    assert_eq!(error(ask(&db, &alice(), DmRequest::Open { token: "t".into(), with: "alice".into() })), "you cannot message yourself");
    assert_eq!(error(ask(&db, &alice(), DmRequest::Open { token: "t".into(), with: "nobody".into() })), "there is no such account");
    let refused = error(ask(&db, &alice(), DmRequest::Send { token: "t".into(), to: "nobody".into(), text: "hi".into(), reply_to: None }));
    assert_eq!(refused, "there is no such account");
}

#[test]
fn messages_need_text_and_stay_within_the_limit() {
    let db = Db::open(":memory:").unwrap();
    open(&db, &alice(), "bob");
    assert_eq!(error(ask(&db, &alice(), DmRequest::Send { token: "t".into(), to: "bob".into(), text: "   ".into(), reply_to: None })), "a message needs some text");
    let long = "x".repeat(common::MAX_DM_TEXT_LEN + 1);
    assert_eq!(error(ask(&db, &alice(), DmRequest::Send { token: "t".into(), to: "bob".into(), text: long, reply_to: None })), "that message is too long");
}

#[test]
fn a_reply_shows_the_message_it_answers_and_must_be_in_the_same_conversation() {
    let db = Db::open(":memory:").unwrap();
    let first = send(&db, &alice(), "bob", "where are we meeting?", None);
    let answer = send(&db, &bob(), "alice", "the usual place", Some(first.id));
    let reply = answer.reply.expect("the reply shows its original");
    assert_eq!(reply.id, first.id);
    assert_eq!(reply.author, "Alice");
    assert_eq!(reply.text, "where are we meeting?");

    open(&db, &alice(), "carol");
    let elsewhere = error(ask(&db, &alice(), DmRequest::Send { token: "t".into(), to: "carol".into(), text: "hi".into(), reply_to: Some(first.id) }));
    assert_eq!(elsewhere, "that message is not in this conversation");
}

#[test]
fn only_the_author_can_edit_or_delete_and_edits_are_marked() {
    let db = Db::open(":memory:").unwrap();
    let id = open(&db, &alice(), "bob");
    let message = send(&db, &alice(), "bob", "helo", None);
    assert_eq!(error(ask(&db, &bob(), DmRequest::Edit { token: "t".into(), id: message.id, text: "x".into() })), "only the author can change or delete this message");
    match ask(&db, &alice(), DmRequest::Edit { token: "t".into(), id: message.id, text: "hello".into() }) {
        DmReply::Changed { message: edited } => {
            assert_eq!(edited.text, "hello");
            assert!(edited.edited);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(error(ask(&db, &bob(), DmRequest::Delete { token: "t".into(), id: message.id })), "only the author can change or delete this message");
    match ask(&db, &alice(), DmRequest::Delete { token: "t".into(), id: message.id }) {
        DmReply::Deleted { channel_id, id: deleted } => {
            assert_eq!((channel_id, deleted), (id, message.id));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_reaction_is_added_then_taken_off_by_the_same_person() {
    let db = Db::open(":memory:").unwrap();
    open(&db, &alice(), "bob");
    let message = send(&db, &alice(), "bob", "nice", None);
    let react = |who: &Identity| ask(&db, who, DmRequest::React { token: "t".into(), id: message.id, emoji: "👍".into() });
    let reacted = |reply: DmReply| match reply {
        DmReply::Changed { message } => message.reactions,
        other => panic!("{other:?}"),
    };
    let once = reacted(react(&bob()));
    assert_eq!(once.len(), 1);
    assert_eq!(once[0].users, vec!["bob".to_string()]);
    let both = reacted(react(&alice()));
    assert_eq!(both[0].users, vec!["bob".to_string(), "alice".to_string()]);
    let taken_off = reacted(react(&bob()));
    assert_eq!(taken_off[0].users, vec!["alice".to_string()]);
}

#[test]
fn a_conversation_can_hold_only_so_many_pins() {
    let db = Db::open(":memory:").unwrap();
    open(&db, &alice(), "bob");
    let mut ids = Vec::new();
    for n in 0..=common::MAX_PINS {
        ids.push(send(&db, &alice(), "bob", &format!("message {n}"), None).id);
    }
    for id in &ids[..common::MAX_PINS] {
        assert!(matches!(ask(&db, &bob(), DmRequest::Pin { token: "t".into(), id: *id, pinned: true }), DmReply::Changed { .. }));
    }
    let full = error(ask(&db, &bob(), DmRequest::Pin { token: "t".into(), id: ids[common::MAX_PINS], pinned: true }));
    assert_eq!(full, "this conversation has as many pinned messages as it can hold");
    assert!(matches!(ask(&db, &bob(), DmRequest::Pin { token: "t".into(), id: ids[0], pinned: false }), DmReply::Changed { .. }));
}

#[test]
fn polling_gives_what_changed_since_the_last_poll() {
    let db = Db::open(":memory:").unwrap();
    open(&db, &alice(), "bob");
    let first = send(&db, &alice(), "bob", "one", None);
    send(&db, &alice(), "bob", "two", None);
    let mut after = match ask(&db, &bob(), DmRequest::Poll { token: "t".into(), after: 0 }) {
        DmReply::Updates { messages, deleted, seq } => {
            assert_eq!(messages.len(), 2);
            assert!(deleted.is_empty());
            seq
        }
        other => panic!("{other:?}"),
    };
    // Nothing has changed since
    match ask(&db, &bob(), DmRequest::Poll { token: "t".into(), after }) {
        DmReply::Updates { messages, deleted, .. } => assert!(messages.is_empty() && deleted.is_empty()),
        other => panic!("{other:?}"),
    }
    // An edit, a reaction and a deletion are each seen once
    ask(&db, &alice(), DmRequest::Edit { token: "t".into(), id: first.id, text: "uno".into() });
    match ask(&db, &bob(), DmRequest::Poll { token: "t".into(), after }) {
        DmReply::Updates { messages, seq, .. } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text, "uno");
            after = seq;
        }
        other => panic!("{other:?}"),
    }
    let second = match ask(&db, &bob(), DmRequest::History { token: "t".into(), channel_id: first.channel_id }) {
        DmReply::Messages { messages } => messages[1].id,
        other => panic!("{other:?}"),
    };
    ask(&db, &alice(), DmRequest::Delete { token: "t".into(), id: second });
    match ask(&db, &bob(), DmRequest::Poll { token: "t".into(), after }) {
        DmReply::Updates { messages, deleted, .. } => {
            assert!(messages.is_empty());
            assert_eq!(deleted, vec![common::DmDeleted { channel_id: first.channel_id, id: second }]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_poll_does_not_show_other_peoples_conversations() {
    let db = Db::open(":memory:").unwrap();
    open(&db, &alice(), "bob");
    send(&db, &alice(), "bob", "for bob", None);
    match ask(&db, &carol(), DmRequest::Poll { token: "t".into(), after: 0 }) {
        DmReply::Updates { messages, deleted, .. } => assert!(messages.is_empty() && deleted.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn read_positions_are_listed_for_the_reader() {
    let db = Db::open(":memory:").unwrap();
    let id = open(&db, &alice(), "bob");
    let message = send(&db, &alice(), "bob", "hi", None);
    assert!(matches!(ask(&db, &bob(), DmRequest::Read { token: "t".into(), channel_id: id, id: message.id }), DmReply::Read));
    match ask(&db, &bob(), DmRequest::List { token: "t".into() }) {
        DmReply::Conversations { read, .. } => assert_eq!(read, vec![common::DmRead { channel_id: id, id: message.id }]),
        other => panic!("{other:?}"),
    }
}
