//! What the direct message service does with each request. It works on the database alone: the
//! caller has already checked who the sender is, and looked up the other person's name, so this part
//! can be tested without a network.

use common::{ChatMessage, DirectMessage, DmDeleted, DmRead, DmReply, DmRequest, Reaction, ReplyRef};

use crate::db::{Conversation, Db, Row};

/// The most messages a history shows.
pub const HISTORY_LEN: i64 = 50;

/// The longest emoji name a reaction may use, in characters.
const MAX_EMOJI_LEN: usize = 32;

/// Who sent the request, as the auth service confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub name: String,
}

/// Answers one request. `other` is the other person's display name, looked up by the caller for requests
/// that name someone (`None` when the account does not exist).
pub fn apply(db: &Db, me: &Identity, request: DmRequest, other: Option<String>) -> DmReply {
    match run(db, me, request, other) {
        Ok(reply) => reply,
        Err(Refused(text)) => DmReply::Error { text },
    }
}

/// A request that could not be done, with the reason for the sender.
struct Refused(String);

impl From<rusqlite::Error> for Refused {
    fn from(e: rusqlite::Error) -> Self {
        Refused(e.to_string())
    }
}

type Outcome = Result<DmReply, Refused>;

fn refuse<T>(text: &str) -> Result<T, Refused> {
    Err(Refused(text.to_string()))
}

fn run(db: &Db, me: &Identity, request: DmRequest, other: Option<String>) -> Outcome {
    // The sender's name is kept, so messages from them show it.
    db.remember_name(&me.id, &me.name)?;
    match request {
        DmRequest::Open { with, .. } => {
            let (with, other) = other_person(&with, me, other)?;
            db.remember_name(&with, &other)?;
            let conversation = db.open_conversation(&me.id, &with)?;
            Ok(DmReply::Opened { conversation: direct(db, &conversation, me)? })
        }
        DmRequest::List { .. } => {
            let mut list = Vec::new();
            for (conversation, _) in db.conversations_of(&me.id)? {
                list.push(direct(db, &conversation, me)?);
            }
            list.sort_by(|x, y| y.last_message.cmp(&x.last_message));
            let read = db
                .reads_of(&me.id)?
                .into_iter()
                .map(|(channel_id, id)| DmRead { channel_id, id })
                .collect();
            Ok(DmReply::Conversations { list, read })
        }
        DmRequest::History { channel_id, .. } => {
            let conversation = member_conversation(db, channel_id, &me.id)?;
            let mut messages = Vec::new();
            for row in db.history(conversation.id, HISTORY_LEN)? {
                messages.push(chat(db, &row)?);
            }
            Ok(DmReply::Messages { messages })
        }
        DmRequest::Send { to, text, reply_to, .. } => {
            let (to, other) = other_person(&to, me, other)?;
            db.remember_name(&to, &other)?;
            let text = clean_text(&text)?;
            let conversation = db.open_conversation(&me.id, &to)?;
            if let Some(reply) = reply_to {
                match db.message(reply)? {
                    Some(target) if target.conversation == conversation.id => {}
                    _ => return refuse("that message is not in this conversation"),
                }
            }
            let row = Row {
                id: 0,
                conversation: conversation.id,
                author: me.id.clone(),
                text,
                timestamp: now(),
                edited: false,
                reply_to: reply_to,
                pinned: false,
            };
            let id = db.insert_message(&row, db.next_seq()?)?;
            changed(db, id)
        }
        DmRequest::Edit { id, text, .. } => {
            let row = own_message(db, id, &me.id)?;
            let text = clean_text(&text)?;
            db.update_text(row.id, &text, db.next_seq()?)?;
            changed(db, row.id)
        }
        DmRequest::Delete { id, .. } => {
            let row = own_message(db, id, &me.id)?;
            db.delete_message(row.id, row.conversation, db.next_seq()?)?;
            Ok(DmReply::Deleted { channel_id: row.conversation, id: row.id })
        }
        DmRequest::React { id, emoji, .. } => {
            let row = message_in_my_conversation(db, id, &me.id)?;
            let emoji = emoji.trim().to_string();
            if emoji.is_empty() || emoji.chars().count() > MAX_EMOJI_LEN {
                return refuse("that is not a reaction");
            }
            db.toggle_reaction(row.id, &me.id, &emoji)?;
            db.touch(row.id, db.next_seq()?)?;
            changed(db, row.id)
        }
        DmRequest::Pin { id, pinned, .. } => {
            let row = message_in_my_conversation(db, id, &me.id)?;
            if pinned && !row.pinned && db.pinned_count(row.conversation)? >= common::MAX_PINS as i64 {
                return refuse("this conversation has as many pinned messages as it can hold");
            }
            db.set_pinned(row.id, pinned, db.next_seq()?)?;
            changed(db, row.id)
        }
        DmRequest::Read { channel_id, id, .. } => {
            member_conversation(db, channel_id, &me.id)?;
            db.set_read(channel_id, &me.id, id)?;
            Ok(DmReply::Read)
        }
        DmRequest::Poll { after, .. } => {
            let mut messages = Vec::new();
            for row in db.changed_since(&me.id, after)? {
                messages.push(chat(db, &row)?);
            }
            let deleted = db
                .deleted_since(&me.id, after)?
                .into_iter()
                .map(|(id, channel_id)| DmDeleted { channel_id, id })
                .collect();
            Ok(DmReply::Updates { messages, deleted, seq: db.current_seq()? })
        }
    }
}

/// The other person, by account id. They must exist, and must not be the sender.
fn other_person(with: &str, me: &Identity, name: Option<String>) -> Result<(String, String), Refused> {
    if with == me.id {
        return refuse("you cannot message yourself");
    }
    match name {
        Some(name) => Ok((with.to_string(), name)),
        None => refuse("there is no such account"),
    }
}

/// The conversation with this id, if the account is one of its two people.
fn member_conversation(db: &Db, id: i64, user: &str) -> Result<Conversation, Refused> {
    match db.conversation(id)? {
        Some(conversation) if conversation.a == user || conversation.b == user => Ok(conversation),
        _ => refuse("that conversation is not yours"),
    }
}

/// A message the account wrote, which it may change or delete.
fn own_message(db: &Db, id: i64, user: &str) -> Result<Row, Refused> {
    let row = message_in_my_conversation(db, id, user)?;
    if row.author != user {
        return refuse("only the author can change or delete this message");
    }
    Ok(row)
}

/// A message in a conversation the account is in.
fn message_in_my_conversation(db: &Db, id: i64, user: &str) -> Result<Row, Refused> {
    let Some(row) = db.message(id)? else {
        return refuse("that message is gone");
    };
    member_conversation(db, row.conversation, user)?;
    Ok(row)
}

fn clean_text(text: &str) -> Result<String, Refused> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return refuse("a message needs some text");
    }
    if text.chars().count() > common::MAX_DM_TEXT_LEN {
        return refuse("that message is too long");
    }
    Ok(text)
}

fn changed(db: &Db, id: i64) -> Outcome {
    let row = db.message(id)?.ok_or_else(|| Refused("that message is gone".into()))?;
    Ok(DmReply::Changed { message: chat(db, &row)? })
}

/// The conversation as the account sees it: the other person, and when the last message was sent.
fn direct(db: &Db, conversation: &Conversation, me: &Identity) -> Result<DirectMessage, Refused> {
    let other_id = if conversation.a == me.id { conversation.b.clone() } else { conversation.a.clone() };
    let name = db.name_of(&other_id)?.unwrap_or_else(|| other_id.clone());
    Ok(DirectMessage {
        channel_id: conversation.id,
        channel: format!("dm-{}", conversation.id),
        name,
        last_message: db.last_timestamp(conversation.id)?,
        other_id,
    })
}

/// A stored message in the shape the clients show, with its reactions and the message it replies to.
fn chat(db: &Db, row: &Row) -> Result<ChatMessage, Refused> {
    let mut reactions: Vec<Reaction> = Vec::new();
    for (emoji, user) in db.reactions(row.id)? {
        match reactions.iter_mut().find(|r| r.emoji == emoji) {
            Some(reaction) => reaction.users.push(user),
            None => reactions.push(Reaction { emoji, users: vec![user] }),
        }
    }
    let reply = match row.reply_to {
        Some(id) => match db.message(id)? {
            Some(target) => Some(ReplyRef {
                id,
                author: db.name_of(&target.author)?.unwrap_or_else(|| target.author.clone()),
                author_id: target.author.clone(),
                text: common::reply_preview(&target.text),
            }),
            None => None,
        },
        None => None,
    };
    Ok(ChatMessage {
        id: row.id,
        channel_id: row.conversation,
        channel: format!("dm-{}", row.conversation),
        author: db.name_of(&row.author)?.unwrap_or_else(|| row.author.clone()),
        author_id: row.author.clone(),
        text: row.text.clone(),
        timestamp: row.timestamp,
        edited: row.edited,
        reactions,
        reply,
        pinned: row.pinned,
        report: None,
        image: None,
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
