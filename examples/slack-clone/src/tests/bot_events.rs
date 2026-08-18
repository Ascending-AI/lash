//! Bot event-semantics tests: dedupe, ambient folding, session isolation, and
//! the standard-mode tool loop.

use std::sync::Arc;

use crate::bot::channel::{Disposition, ReplySource};
use crate::bot::ledger::Stage;
use crate::bot::runtime::{session_id, thread_session_id};
use crate::bot::tools::{CHANNEL_HISTORY, LIST_CHANNELS};

use super::support::{Script, Step, TestPlatform, bot_dir, only_event, scratch, start_bot};

#[tokio::test]
async fn a_mention_runs_one_turn_and_posts_one_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("I am here.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("mention-turn").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} are you there?"))
        .await;

    let envelopes = platform.drain_envelopes().await;
    let app_mention = only_event(&envelopes, "app_mention");
    let disposition = bot
        .ingest(app_mention.clone(), None)
        .await
        .expect("handle app_mention");
    let Disposition::Replied {
        reply_ts, source, ..
    } = disposition
    else {
        panic!("expected a reply, got {disposition:?}");
    };
    assert_eq!(
        source,
        ReplySource::Turn,
        "the first delivery must run the model"
    );
    assert_eq!(script.calls(), 1);

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].text, "I am here.");
    assert_eq!(replies[0].ts, reply_ts);
    // The reply carries the originating event id, which is what makes recovery a
    // read rather than a guess.
    let metadata = replies[0].metadata.as_ref().expect("reply metadata");
    assert_eq!(metadata.event_type, "slack_clone_bot_reply");
    assert_eq!(
        metadata.event_payload["event_id"],
        serde_json::json!(app_mention.event_id)
    );

    // The bot's own mention token is stripped and the author is named, so the
    // model sees a line of conversation rather than Slack markup.
    assert!(
        script.saw("ada: are you there?"),
        "prompt should carry the resolved mention text: {:?}",
        script.requests()
    );
    assert!(
        !script.saw(&mention),
        "the bot's own mention token must not reach the model"
    );
}

#[tokio::test]
async fn the_same_event_id_delivered_twice_runs_one_turn_and_posts_one_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Once only.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("dedupe").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hello"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let first = bot
        .ingest(app_mention.clone(), None)
        .await
        .expect("first delivery");
    // Slack's retries carry the same event id and an `x-slack-retry-num` header.
    let second = bot
        .ingest(app_mention.clone(), Some(1))
        .await
        .expect("redelivery");
    let third = bot
        .ingest(app_mention.clone(), Some(2))
        .await
        .expect("second redelivery");

    assert!(matches!(
        first,
        Disposition::Replied {
            source: ReplySource::Turn,
            ..
        }
    ));
    for redelivery in [&second, &third] {
        let Disposition::Duplicate { stage, .. } = redelivery else {
            panic!("a redelivery must not act: {redelivery:?}");
        };
        assert_eq!(*stage, Stage::Replied);
    }
    assert_eq!(script.calls(), 1, "exactly one turn for three deliveries");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);

    let record = bot
        .ledger()
        .get(app_mention.event_id.clone())
        .await
        .expect("read ledger")
        .expect("ledger row");
    assert_eq!(record.deliveries, 3, "every delivery is counted");
    assert_eq!(record.stage, Stage::Replied);
}

#[tokio::test]
async fn ambient_traffic_folds_into_the_session_without_a_turn_or_a_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Caught up.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("ambient").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;
    platform.say(&channel, &ada, "the deploy is stuck").await;
    platform.say(&channel, &grace, "rolling it back now").await;

    let mut folded = Vec::new();
    for envelope in platform.drain_envelopes().await {
        folded.push(bot.ingest(envelope, None).await.expect("handle message"));
    }
    assert_eq!(folded.len(), 2);
    for disposition in &folded {
        assert!(
            matches!(disposition, Disposition::Folded { .. }),
            "ambient traffic folds: {disposition:?}"
        );
    }
    assert_eq!(script.calls(), 0, "no turn runs for ambient traffic");
    assert!(
        platform.bot_messages(&channel).await.is_empty(),
        "and no reply is posted"
    );

    // Lash's own evidence: two durable admissions waiting for the next turn.
    let session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    let pending = session
        .pending_turn_inputs()
        .await
        .expect("read pending turn inputs");
    assert_eq!(
        pending.len(),
        2,
        "both ambient lines are queued: {pending:?}"
    );

    // Now a mention: one drain folds the room context and the mention into one turn.
    let mention = platform.mention();
    platform
        .say(&channel, &ada, &format!("{mention} what happened?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    assert!(matches!(disposition, Disposition::Replied { .. }));
    assert_eq!(script.calls(), 1, "one turn, not one per queued line");
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("read pending turn inputs")
            .is_empty(),
        "the drain consumed every queued input"
    );
    assert!(
        script.saw("the deploy is stuck") && script.saw("rolling it back now"),
        "the ambient context must reach the prompt: {:?}",
        script.requests()
    );
}

#[tokio::test]
async fn the_bot_ignores_app_authored_messages_and_its_own_replies() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Hello back.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("self-guard").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hi"))
        .await;

    // The mention arrives as *two* events. Handling the `message` twin as well
    // must not produce a second turn.
    let envelopes = platform.drain_envelopes().await;
    let mut dispositions = Vec::new();
    for envelope in envelopes {
        dispositions.push(bot.ingest(envelope, None).await.expect("handle event"));
    }
    let ignored = dispositions
        .iter()
        .filter(|disposition| {
            matches!(
                disposition,
                Disposition::Ignored {
                    reason: "superseded_by_app_mention",
                    ..
                }
            )
        })
        .count();
    assert_eq!(ignored, 1, "the message twin of a mention is dropped");
    assert_eq!(script.calls(), 1, "one turn for one mention");

    // The bot's own reply generated an event too. Feeding it back must be inert —
    // otherwise the bot answers itself forever.
    let after_reply = platform.drain_envelopes().await;
    assert!(!after_reply.is_empty(), "the reply produced an event");
    for envelope in after_reply {
        let disposition = bot.ingest(envelope, None).await.expect("handle own reply");
        assert!(
            matches!(
                disposition,
                Disposition::Ignored {
                    reason: "app_authored_message",
                    ..
                }
            ),
            "app-authored messages are inert: {disposition:?}"
        );
    }
    assert_eq!(script.calls(), 1, "still one turn");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
}

#[tokio::test]
async fn each_channel_gets_its_own_session_and_neither_sees_the_others_context() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Noted.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let secrets = platform.channel("secrets").await;
    let public = platform.channel("public").await;
    let ada = platform.identify("ada").await;
    platform
        .say(&secrets, &ada, "the passphrase is hunter2")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold secret");
    }

    let mention = platform.mention();
    platform
        .say(&public, &ada, &format!("{mention} anything to share?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(app_mention, None).await.expect("handle mention");

    assert!(
        !script.saw("hunter2"),
        "#secrets context must not leak into #public's session: {:?}",
        script.requests()
    );

    let secret_session = bot
        .core()
        .session(session_id(&secrets))
        .open()
        .await
        .expect("open secrets session");
    assert_eq!(
        secret_session
            .pending_turn_inputs()
            .await
            .expect("pending inputs")
            .len(),
        1,
        "the other channel's queued context is untouched"
    );
    let public_session = bot
        .core()
        .session(session_id(&public))
        .open()
        .await
        .expect("open public session");
    assert!(
        public_session
            .pending_turn_inputs()
            .await
            .expect("pending inputs")
            .is_empty()
    );
    assert_ne!(secret_session.session_id(), public_session.session_id());
}

#[tokio::test]
async fn a_thread_forks_on_its_first_reply_and_inherits_uncommitted_root_context() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("The deploy target was EU west.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("thread-fork").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;
    let root = platform
        .say(&channel, &ada, "the deploy target is EU west")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold thread root");
    }

    let channel_session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    let channel_head_before = channel_session
        .read_view()
        .session_graph()
        .leaf_node_id
        .clone();

    let mention = platform.mention();
    platform
        .say_thread(
            &channel,
            &grace,
            root,
            &format!("{mention} where are we deploying?"),
        )
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot
        .ingest(app_mention, None)
        .await
        .expect("handle first thread engagement");
    assert!(matches!(disposition, Disposition::Replied { .. }));

    let thread_id = thread_session_id(&channel, &root.to_string());
    assert!(
        bot.core()
            .session_exists(&thread_id)
            .await
            .expect("check child session existence"),
        "the deterministic child session must exist"
    );
    assert_eq!(
        channel_session.read_view().session_graph().leaf_node_id,
        channel_head_before,
        "forking and running the thread must not advance the channel head"
    );
    assert!(
        script.saw("the deploy target is EU west"),
        "the queued root is copied because it was not yet in the forked graph: {:?}",
        script.requests()
    );
    assert_eq!(platform.thread_messages(&channel, root).await.len(), 2);
    assert!(
        platform.bot_messages(&channel).await.is_empty(),
        "the bot reply is in the thread, not channel history"
    );
}

#[tokio::test]
async fn a_thread_reply_waits_for_midflight_root_admission_and_forks_from_that_turn() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Gated("Root admission completed.".to_string()),
        Step::Text("Thread answer from the root boundary.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-root-admission-race").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;

    let root = platform
        .say(
            &channel,
            &ada,
            &format!("{} establish the root", platform.mention()),
        )
        .await;
    let root_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let root_turn = tokio::spawn({
        let bot = Arc::clone(&bot);
        async move { bot.ingest(root_mention, None).await }
    });
    script.wait_gated().await;

    platform
        .say_thread(
            &channel,
            &grace,
            root,
            &format!("{} answer from this thread", platform.mention()),
        )
        .await;
    let reply_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let mut reply_turn = tokio::spawn({
        let bot = Arc::clone(&bot);
        async move { bot.ingest(reply_mention, None).await }
    });

    tokio::select! {
        () = bot.wait_for_missing_thread_root() => {}
        outcome = &mut reply_turn => {
            panic!("thread turn completed before its root became durable: {outcome:?}");
        }
    }
    let thread_id = thread_session_id(&channel, &root.to_string());
    assert!(
        !bot.core()
            .session_exists(&thread_id)
            .await
            .expect("check child before root completion"),
        "missing-root deferral must not fork from the channel's current leaf"
    );

    script.release_gate();
    let root_disposition = root_turn
        .await
        .expect("join root turn")
        .expect("complete root turn");
    assert!(matches!(root_disposition, Disposition::Replied { .. }));
    let reply_disposition = reply_turn
        .await
        .expect("join thread turn")
        .expect("complete deferred thread turn");
    assert!(matches!(reply_disposition, Disposition::Replied { .. }));

    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].contains("Root admission completed."),
        "the child must share the completed root turn, not fork from the pre-root leaf: {}",
        requests[1]
    );
}

#[tokio::test]
async fn a_permanently_missing_root_fails_loudly_then_root_arrival_and_retry_recover() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Recovered from the authoritative root.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    bot.set_thread_root_wait_budget(std::time::Duration::ZERO);
    let channel = platform.channel("thread-missing-root-recovery").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;

    let root = platform
        .say(&channel, &ada, "authoritative root fact")
        .await;
    let root_event = only_event(&platform.drain_envelopes().await, "message");
    platform
        .say_thread(
            &channel,
            &grace,
            root,
            &format!("{} use the root", platform.mention()),
        )
        .await;
    let reply_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let failure = bot
        .ingest(reply_mention.clone(), None)
        .await
        .expect("exhaust bounded root wait");
    assert!(matches!(
        failure,
        Disposition::RecoverableFailure {
            notified: true,
            reason: "thread_root_not_available",
            ..
        }
    ));
    assert_eq!(script.calls(), 0, "no turn can run without its root");
    let failed_record = bot
        .ledger()
        .get(reply_mention.event_id.clone())
        .await
        .expect("read recoverable failure")
        .expect("recoverable failure row");
    assert_eq!(failed_record.stage, Stage::Accepted);
    assert!(!failed_record.stage.is_terminal());
    assert_eq!(
        failed_record.detail.as_deref(),
        Some("thread_root_not_available")
    );
    let replies = platform.thread_messages(&channel, root).await;
    let error_reply = replies
        .iter()
        .find(|message| message.bot_id.is_some())
        .expect("in-thread missing-root notification");
    assert!(
        error_reply
            .text
            .contains("can’t find the message this thread started from")
    );
    assert!(error_reply.text.contains("follow up here"));

    let second_failure = bot
        .ingest(reply_mention.clone(), Some(1))
        .await
        .expect("exhaust the still-missing root a second time");
    assert!(matches!(
        second_failure,
        Disposition::RecoverableFailure {
            notified: false,
            reason: "thread_root_not_available",
            ..
        }
    ));
    assert_eq!(
        platform
            .thread_messages(&channel, root)
            .await
            .iter()
            .filter(|message| message.bot_id.is_some())
            .count(),
        1,
        "second exhaustion must find the first notification by metadata identity"
    );

    let folded = bot
        .ingest(root_event, None)
        .await
        .expect("admit the late root");
    assert!(matches!(folded, Disposition::Folded { .. }));
    let recovered = bot
        .ingest(reply_mention.clone(), Some(1))
        .await
        .expect("retry after root arrival");
    assert!(matches!(
        recovered,
        Disposition::Replied {
            source: ReplySource::Turn,
            ..
        }
    ));
    assert_eq!(script.calls(), 1);
    assert!(script.saw("authoritative root fact"));
    let recovered_record = bot
        .ledger()
        .get(reply_mention.event_id)
        .await
        .expect("read recovered row")
        .expect("recovered row");
    assert_eq!(recovered_record.stage, Stage::Replied);
    assert_eq!(
        platform
            .thread_messages(&channel, root)
            .await
            .iter()
            .filter(|message| message.bot_id.is_some())
            .count(),
        2,
        "one loud failure and one recovered answer"
    );
}

#[tokio::test]
async fn a_terminal_unroutable_root_fails_fast_without_spending_the_wait_budget() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("must not run");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    bot.set_thread_root_wait_budget(std::time::Duration::from_secs(60));
    let channel = platform.channel("thread-terminal-root").await;
    let ada = platform.identify("ada").await;

    let root = platform.say(&channel, &ada, "authorless root").await;
    let mut root_event = only_event(&platform.drain_envelopes().await, "message");
    let crate::wire::events::Event::Message(message) = &mut root_event.event else {
        unreachable!("say emits a message event")
    };
    message.user = None;
    let ignored = bot
        .ingest(root_event, None)
        .await
        .expect("record the permanently unroutable root");
    assert!(matches!(
        ignored,
        Disposition::Ignored {
            reason: "no_author",
            ..
        }
    ));

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} can this route?", platform.mention()),
        )
        .await;
    let reply = only_event(&platform.drain_envelopes().await, "app_mention");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), bot.ingest(reply, None))
        .await
        .expect("terminal root evidence must bypass the 60s wait")
        .expect("handle unroutable-root reply");
    assert!(matches!(
        outcome,
        Disposition::RecoverableFailure {
            reason: "thread_root_not_available",
            ..
        }
    ));
    assert_eq!(script.calls(), 0);
}

#[tokio::test]
async fn a_thread_fork_shares_a_committed_channel_turn_by_provenance() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Text("Channel answer.".to_string()),
        Step::Text("Thread answer.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-committed-root").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "retained channel fact").await;
    let root_event = only_event(&platform.drain_envelopes().await, "message");
    bot.ingest(root_event.clone(), None)
        .await
        .expect("fold root");

    platform
        .say(
            &channel,
            &ada,
            &format!("{} commit the channel turn", platform.mention()),
        )
        .await;
    let channel_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(channel_mention, None)
        .await
        .expect("commit channel turn");
    let root_record = bot
        .ledger()
        .get(root_event.event_id)
        .await
        .expect("read ledger")
        .expect("root row");
    assert!(
        root_record.input_id.is_some() && root_record.fork_node_id.is_some(),
        "typed application correlation records the retained boundary: {root_record:?}"
    );

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} what was retained?", platform.mention()),
        )
        .await;
    let thread_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(thread_mention, None)
        .await
        .expect("run forked thread turn");
    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].contains("retained channel fact"),
        "the child shares committed ancestry: {}",
        requests[1]
    );
}

#[tokio::test]
async fn the_child_prompt_names_its_thread_root_among_the_inherited_prefix() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Text("Channel answer.".to_string()),
        Step::Text("Thread answer.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-root-recall").await;
    let ada = platform.identify("ada").await;

    // The root, then unrelated room traffic, then the mention that drains all
    // three into one committed channel turn. The child forks at that turn's
    // boundary, so its inherited prefix extends well past the root.
    let root = platform.say(&channel, &ada, "the root says cobalt").await;
    platform
        .say(&channel, &ada, "someone else says cedar")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold room traffic");
    }
    platform
        .say(
            &channel,
            &ada,
            &format!("{} recall the room", platform.mention()),
        )
        .await;
    let channel_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(channel_mention, None)
        .await
        .expect("commit channel turn");

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} what did the root say?", platform.mention()),
        )
        .await;
    let thread_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(thread_mention, None)
        .await
        .expect("run forked thread turn");

    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    // Inheritance alone is not the property: all three channel lines are in the
    // prefix. The child must be told which one the thread hangs from.
    assert!(
        requests[1].contains("the root says cobalt")
            && requests[1].contains("someone else says cedar")
            && requests[1].contains("recall the room"),
        "the child inherits the committed channel turn: {}",
        requests[1]
    );
    let seed = format!(
        "{}ada: the root says cobalt",
        crate::bot::threads::THREAD_ROOT_SEED_PREFIX
    );
    assert!(
        requests[1].contains(&seed),
        "the host must seed the thread root so the child can tell it apart: {}",
        requests[1]
    );
    assert!(
        !requests[1].contains(&format!(
            "{}ada: someone else says cedar",
            crate::bot::threads::THREAD_ROOT_SEED_PREFIX
        )),
        "only the root is seeded: {}",
        requests[1]
    );
    assert!(
        label_always_starts_a_line(&requests[1], crate::bot::threads::THREAD_ROOT_SEED_PREFIX),
        "the seed label must start its own line: {}",
        requests[1]
    );
}

/// Whether every occurrence of `label` in the request's text starts a line.
///
/// Queued text inputs concatenate into one user message with no separator, so a
/// label is only readable as a label when nothing precedes it on its line. The
/// request is inspected as JSON because that is how the scripted provider
/// records it — searching the encoded string would compare against `\n` escapes.
fn label_always_starts_a_line(request: &str, label: &str) -> bool {
    let mut stack = vec![serde_json::from_str::<serde_json::Value>(request).expect("request json")];
    let mut seen = false;
    let mut all_at_line_start = true;
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::String(text) => {
                for (index, _) in text.match_indices(label) {
                    seen = true;
                    all_at_line_start &= index == 0 || text.as_bytes()[index - 1] == b'\n';
                }
            }
            serde_json::Value::Array(items) => stack.extend(items),
            serde_json::Value::Object(fields) => stack.extend(fields.into_values()),
            _ => {}
        }
    }
    seen && all_at_line_start
}

#[tokio::test]
async fn the_seeded_root_label_starts_its_line_behind_copied_queued_context() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Thread answer.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-queued-root-label").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;

    // Nothing drains the channel, so both lines are still queued when the thread
    // forks: the earlier line is copied into the child verbatim and the root
    // follows it as the labelled seed, with no separator of Lash's making.
    platform
        .say(&channel, &ada, "the earlier line mentions basalt")
        .await;
    let root = platform
        .say(&channel, &ada, "the deploy target is EU west")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold room traffic");
    }

    platform
        .say_thread(
            &channel,
            &grace,
            root,
            &format!("{} where are we deploying?", platform.mention()),
        )
        .await;
    let thread_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(thread_mention, None)
        .await
        .expect("run forked thread turn");

    let requests = script.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("the earlier line mentions basalt"),
        "the copied queued context precedes the seed: {}",
        requests[0]
    );
    assert!(
        label_always_starts_a_line(&requests[0], crate::bot::threads::THREAD_ROOT_SEED_PREFIX),
        "the seed label must not run out of the line copied ahead of it: {}",
        requests[0]
    );
}

#[tokio::test]
async fn recovery_records_the_applied_turns_boundary_after_a_later_turn_commits() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let script = Script::new([
        Step::Text("Older answer.".to_string()),
        Step::Text("Later answer.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir, &script).await;
    let channel = platform.channel("recovery-retains-applied-turn").await;
    let ada = platform.identify("ada").await;

    let root = platform.say(&channel, &ada, "older retained fact").await;
    let root_event = only_event(&platform.drain_envelopes().await, "message");
    bot.ingest(root_event.clone(), None)
        .await
        .expect("fold older root");
    platform
        .say(
            &channel,
            &ada,
            &format!("{} commit older turn", platform.mention()),
        )
        .await;
    let older_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let older_reply = bot
        .ingest(older_mention.clone(), None)
        .await
        .expect("commit older turn");
    let Disposition::Replied {
        reply_ts: older_reply_ts,
        ..
    } = older_reply
    else {
        panic!("older mention must reply: {older_reply:?}");
    };
    let older_boundary = bot
        .ledger()
        .get(root_event.event_id.clone())
        .await
        .expect("read older root")
        .expect("older root row")
        .fork_node_id
        .expect("older turn boundary was initially recorded");

    // Reconstruct a crash after pinning the committed boundary but before the
    // additive ledger write records it. The reply and terminal stage are also
    // absent, so the next boot must take the committed empty-drain path.
    platform.delete_message(&channel, &older_reply_ts).await;
    bot.ledger()
        .advance(
            older_mention.event_id.clone(),
            Stage::Replied,
            Stage::Accepted,
            None,
            None,
        )
        .await
        .expect("rewind older event to the crash window");
    rusqlite::Connection::open(bot_dir.join("events.db"))
        .expect("open ledger for crash staging")
        .execute(
            "UPDATE event_routes SET fork_node_id = NULL WHERE fork_node_id = ?1",
            rusqlite::params![older_boundary],
        )
        .expect("remove the unrecorded boundary");

    platform
        .say(
            &channel,
            &ada,
            &format!("{} commit later turn", platform.mention()),
        )
        .await;
    let later_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(later_mention.clone(), None)
        .await
        .expect("commit later turn");
    let later_boundary = bot
        .ledger()
        .get(later_mention.event_id)
        .await
        .expect("read later mention")
        .expect("later mention row")
        .fork_node_id
        .expect("later turn boundary");
    assert_ne!(
        older_boundary, later_boundary,
        "the scenario must stage two distinct committed turn boundaries"
    );

    let recovered = bot.recover().await.expect("recover older turn").settled;
    assert_eq!(recovered.len(), 1, "only the older turn was unfinished");
    let recovered_root = bot
        .ledger()
        .get(root_event.event_id)
        .await
        .expect("read recovered root")
        .expect("recovered root row");
    assert_eq!(
        recovered_root.fork_node_id.as_deref(),
        Some(older_boundary.as_str()),
        "recovery must retain the older applied turn, not the channel's current head"
    );
    assert_ne!(
        recovered_root.fork_node_id.as_deref(),
        Some(later_boundary.as_str()),
        "post-root channel content must not become part of the root fork"
    );
    assert_eq!(root.to_string(), recovered_root.message_ts);
}

#[tokio::test]
async fn thread_open_rederives_a_missing_root_boundary_from_its_application() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let script = Script::new([
        Step::Text("Root turn answer.".to_string()),
        Step::Text("Later channel answer.".to_string()),
        Step::Text("Thread answer.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir, &script).await;
    let channel = platform.channel("thread-rederives-root-boundary").await;
    let ada = platform.identify("ada").await;

    let root = platform.say(&channel, &ada, "root fact before fork").await;
    let root_event = only_event(&platform.drain_envelopes().await, "message");
    bot.ingest(root_event.clone(), None)
        .await
        .expect("fold root fact");
    platform
        .say(
            &channel,
            &ada,
            &format!("{} commit root turn", platform.mention()),
        )
        .await;
    let root_turn = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(root_turn, None).await.expect("commit root turn");
    let recorded_root = bot
        .ledger()
        .get(root_event.event_id.clone())
        .await
        .expect("read committed root")
        .expect("committed root row");
    let root_boundary = recorded_root
        .fork_node_id
        .expect("root boundary initially recorded");
    assert!(
        recorded_root.input_id.is_some(),
        "root keeps durable input identity"
    );

    // Crash between pin and record_fork_node_for_inputs: the retained point and
    // application survive, but this root route lacks its fork-node projection.
    rusqlite::Connection::open(bot_dir.join("events.db"))
        .expect("open ledger for crash staging")
        .execute(
            "UPDATE event_routes SET fork_node_id = NULL WHERE event_id = ?1",
            rusqlite::params![root_event.event_id],
        )
        .expect("remove root fork-node projection");

    platform
        .say(
            &channel,
            &ada,
            &format!("{} later channel turn", platform.mention()),
        )
        .await;
    let later_turn = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(later_turn, None)
        .await
        .expect("commit later channel turn");

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} open from root", platform.mention()),
        )
        .await;
    let thread_turn = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(thread_turn, None)
        .await
        .expect("open thread from rederived root");

    let requests = script.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2].contains("root fact before fork"),
        "the rederived boundary includes the root turn: {}",
        requests[2]
    );
    assert!(
        !requests[2].contains("later channel turn"),
        "thread-open must not silently degrade a repairable root to the current head: {}",
        requests[2]
    );
    let repaired_root = bot
        .ledger()
        .get(root_event.event_id)
        .await
        .expect("read repaired root")
        .expect("repaired root row");
    assert_eq!(
        repaired_root.fork_node_id.as_deref(),
        Some(root_boundary.as_str()),
        "thread-open repairs the durable root projection"
    );
}

#[tokio::test]
async fn thread_and_channel_traffic_are_isolated_after_the_fork() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Text("Thread answer one.".to_string()),
        Step::Text("Thread answer two.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-isolation").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "shared before fork").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }

    let mention = platform.mention();
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{mention} first thread turn"),
        )
        .await;
    let first = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(first, None).await.expect("first thread turn");

    platform
        .say(&channel, &ada, "channel only after fork")
        .await;
    for envelope in platform.drain_envelopes().await {
        if envelope.event.thread_ts().is_none() {
            bot.ingest(envelope, None)
                .await
                .expect("fold channel-only traffic");
        }
    }
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{mention} second thread turn"),
        )
        .await;
    let second = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(second, None).await.expect("second thread turn");

    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("first thread turn"));
    assert!(
        !requests[1].contains("channel only after fork"),
        "post-fork channel traffic cannot appear in the child prompt: {}",
        requests[1]
    );

    let channel_text = channel_session_text(&bot, &channel).await;
    assert_eq!(
        channel_text, "",
        "queued channel inputs and thread turns must not become committed channel transcript"
    );
    assert!(!channel_text.contains("first thread turn"));
    assert!(!channel_text.contains("second thread turn"));
    let channel_session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    assert_eq!(
        channel_session
            .pending_turn_inputs()
            .await
            .expect("channel pending inputs")
            .len(),
        2,
        "the root and post-fork channel traffic remain queued only on the channel session"
    );
    assert_eq!(
        bot.session_lock_count(),
        0,
        "uncontended routed-session locks are scoped to active handling"
    );
}

#[tokio::test]
async fn a_thread_event_is_deduplicated_in_the_shared_ledger() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Once in thread.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-dedupe").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "root").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} answer once", platform.mention()),
        )
        .await;
    let event = only_event(&platform.drain_envelopes().await, "app_mention");
    assert!(matches!(
        bot.ingest(event.clone(), None)
            .await
            .expect("first delivery"),
        Disposition::Replied { .. }
    ));
    assert!(matches!(
        bot.ingest(event, Some(1)).await.expect("redelivery"),
        Disposition::Duplicate {
            stage: Stage::Replied,
            ..
        }
    ));
    assert_eq!(script.calls(), 1);
    assert_eq!(platform.thread_messages(&channel, root).await.len(), 2);
}

#[tokio::test]
async fn an_ambient_thread_reply_creates_the_fork_and_waits_for_a_mention() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("I saw the thread context.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-ambient").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "channel root fact").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }

    platform
        .say_thread(&channel, &ada, root, "thread detail before engagement")
        .await;
    let ambient = only_event(&platform.drain_envelopes().await, "message");
    assert!(matches!(
        bot.ingest(ambient, None)
            .await
            .expect("fold thread ambient"),
        Disposition::Folded { .. }
    ));
    assert_eq!(script.calls(), 0, "ambient thread traffic spends no token");
    let thread = bot
        .core()
        .session(thread_session_id(&channel, &root.to_string()))
        .open()
        .await
        .expect("open fork created by first reply");
    assert_eq!(
        thread
            .pending_turn_inputs()
            .await
            .expect("thread pending inputs")
            .len(),
        2,
        "the inherited root and ambient thread reply wait in the child"
    );

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} now answer", platform.mention()),
        )
        .await;
    let mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(mention, None)
        .await
        .expect("drain thread context");
    assert!(script.saw("channel root fact"));
    assert!(script.saw("thread detail before engagement"));
}

async fn channel_session_text(bot: &crate::bot::channel::ChannelBot, channel: &str) -> String {
    let session = bot
        .core()
        .session(session_id(channel))
        .open()
        .await
        .expect("open channel session");
    session
        .read_view()
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                Some(lash::message_text(&message))
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_mention_can_drive_the_standard_tool_loop() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    // Two native tool calls, then prose: the standard-mode loop, twice around.
    let script = Script::new([
        Step::ToolCall {
            name: LIST_CHANNELS.to_string(),
            args: serde_json::json!({}),
        },
        Step::ToolCall {
            name: CHANNEL_HISTORY.to_string(),
            args: serde_json::json!({ "channel": "tooling", "limit": 5 }),
        },
        Step::Text("There are channels, and #tooling has traffic.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("tooling").await;
    let user = platform.identify("ada").await;
    platform.say(&channel, &user, "background chatter").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold chatter");
    }

    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} what channels exist?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    let Disposition::Replied { .. } = disposition else {
        panic!("expected a reply, got {disposition:?}");
    };
    assert_eq!(
        script.calls(),
        3,
        "two tool calls plus the final answer is three provider calls"
    );

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].text,
        "There are channels, and #tooling has traffic."
    );
    // The tools really called the platform: the channel the test created and the
    // message it posted both came back through `conversations.*`.
    let requests = script.requests().join("\n");
    assert!(
        requests.contains("tooling"),
        "list_channels output should name the channel"
    );
    assert!(
        requests.contains("background chatter"),
        "channel_history output should carry the posted message"
    );
}

#[tokio::test]
async fn an_envelope_with_the_wrong_verification_token_is_rejected() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("never");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("spoofed").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hi"))
        .await;
    let mut app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    app_mention.token = "not-the-verification-token".to_string();

    let disposition = bot.ingest(app_mention, None).await.expect("handle spoof");
    assert_eq!(
        disposition,
        Disposition::Rejected {
            reason: "bad_verification_token"
        }
    );
    assert_eq!(script.calls(), 0);
    assert!(platform.bot_messages(&channel).await.is_empty());
}

#[tokio::test]
async fn an_empty_model_answer_is_absorbed_rather_than_posted() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("   ");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("quiet").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform.say(&channel, &user, &format!("{mention} ?")).await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    assert!(
        matches!(
            disposition,
            Disposition::Silent {
                reason: "empty_model_reply",
                ..
            }
        ),
        "an empty answer must not become an empty message: {disposition:?}"
    );
    assert!(platform.bot_messages(&channel).await.is_empty());
}
