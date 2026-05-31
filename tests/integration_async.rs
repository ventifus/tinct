//! Integration tests for async channel protocol (Phase 3).
//!
//! These tests verify the channel protocol redesign from data-streaming Phase 3:
//! - `recv` returns [Ok v] on success, [Closed] when channel is closed
//! - `select-once` returns [Ok v] on success, [Closed] when all sources closed
//! - `try-send` returns [Ok], [Full], or [Closed] without blocking
//! - `broadcast-channel` multi-subscriber semantics
//! - `oneshot-channel` single-use request/response pattern
//!
//! Why integration tests and not corpus tests?
//! Corpus tests require serializable output. Channel values cannot be serialized
//! (error E035 "cannot serialize Channel"). Testing channel protocol requires:
//! 1. Creating a channel
//! 2. Sending/receiving values
//! 3. Inspecting the result variant structure ([Ok v] vs [Closed])
//!
//! Corpus tests hit E035 when trying to serialize a dict containing a Channel.
//! Integration tests use tokio::test infrastructure and eval_source_with_config
//! to test the full async protocol without serialization constraints.

#![cfg(feature = "cli")]

use tinct::eval_source_with_config;

// ---------------------------------------------------------------------------
// recv protocol: [Ok v] / [Closed]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recv_success_returns_ok_variant() {
    // Create a channel, send a value, then recv — should return [Ok 42].
    // The variant structure is a Dict with tag "Ok" and a value field.
    let source = r#"
[
  ch: [builtin-channel 1]
  _: [builtin-send $ch 42]
  result: [builtin-recv $ch]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Ok".
    // DisplayVisitor format: Dict({"@": String("Ok"), "tag": String("Ok"), ...})
    assert!(
        output.contains("Ok"),
        "expected recv to return [Ok v] variant; got: {output}"
    );
    assert!(
        output.contains("42"),
        "expected recv payload to be 42; got: {output}"
    );
}

#[tokio::test]
async fn recv_on_closed_channel_returns_closed_variant() {
    // Create a channel, drop the sender side (by letting it go out of scope),
    // then recv — should return [Closed].
    //
    // Current limitation: LLT channels are unified (not split sender/receiver).
    // [builtin-channel N] returns a single Channel value. To test [Closed],
    // we need a way to close the channel. The oneshot-channel builtin provides
    // separate sender/receiver handles, so we use that for this test.
    let source = r#"
[
  pair: [builtin-oneshot-channel]
  sender: $pair.sender
  receiver: $pair.receiver
  _: [builtin-close-sender $sender]
  result: [builtin-recv $receiver]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Closed".
    // DisplayVisitor format: Dict({"@": String("Closed"), "tag": String("Closed"), ...})
    assert!(
        output.contains("Closed"),
        "expected recv on closed channel to return [Closed]; got: {output}"
    );
}

// ---------------------------------------------------------------------------
// try-send protocol: [Ok] / [Full] / [Closed]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn try_send_success_returns_ok_variant() {
    // Create a channel with capacity 2, try-send a value — should return [Ok].
    let source = r#"
[
  ch: [builtin-channel 2]
  result: [builtin-try-send $ch 42]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Ok".
    // DisplayVisitor format: Dict({"@": String("Ok"), ...})
    assert!(
        output.contains("Ok"),
        "expected try-send to return [Ok] on success; got: {output}"
    );
}

#[tokio::test]
async fn try_send_on_full_channel_returns_full_variant() {
    // Create a channel with capacity 1, send one value, then try-send again — should return [Full].
    let source = r#"
[
  ch: [builtin-channel 1]
  _: [builtin-send $ch 1]
  result: [builtin-try-send $ch 2]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Full".
    // DisplayVisitor format: Dict({"@": String("Full"), ...})
    assert!(
        output.contains("Full"),
        "expected try-send to return [Full] when channel is full; got: {output}"
    );
}

#[tokio::test]
async fn try_send_on_closed_channel_returns_closed_variant() {
    // Create a oneshot channel, close the sender, then try-send — should return [Closed].
    let source = r#"
[
  pair: [builtin-oneshot-channel]
  sender: $pair.sender
  _: [builtin-close-sender $sender]
  result: [builtin-try-send $sender 42]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Closed".
    // DisplayVisitor format: Dict({"@": String("Closed"), ...})
    assert!(
        output.contains("Closed"),
        "expected try-send on closed channel to return [Closed]; got: {output}"
    );
}

// ---------------------------------------------------------------------------
// oneshot-channel single-use semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oneshot_channel_single_send_recv() {
    // Create a oneshot channel, send once, recv once — should work.
    // The second recv should return [Closed].
    let source = r#"
[
  pair: [builtin-oneshot-channel]
  sender: $pair.sender
  receiver: $pair.receiver
  _: [builtin-send $sender 42]
  first-recv: [builtin-recv $receiver]
  second-recv: [builtin-recv $receiver]
  first-tag: $first-recv.@
  second-tag: $second-recv.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // First recv should return [Ok 42]
    assert!(
        output.contains("Ok"),
        "expected first recv to return [Ok v]; got: {output}"
    );
    assert!(
        output.contains("42"),
        "expected first recv payload to be 42; got: {output}"
    );

    // Second recv should return [Closed]
    assert!(
        output.contains("Closed"),
        "expected second recv to return [Closed]; got: {output}"
    );
}

// ---------------------------------------------------------------------------
// broadcast-channel multi-subscriber semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn broadcast_channel_multi_subscriber() {
    // Create a broadcast channel, subscribe twice, send a value — both subscribers should receive it.
    let source = r#"
[
  bcast: [builtin-broadcast-channel 2]
  sub1: [builtin-subscribe $bcast]
  sub2: [builtin-subscribe $bcast]
  _: [builtin-send $bcast 42]
  recv1: [builtin-recv $sub1]
  recv2: [builtin-recv $sub2]
  tag1: $recv1.@
  tag2: $recv2.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Both recv calls should return [Ok 42]
    // We expect to see "Ok" and "42" appear at least twice in the output
    let ok_count = output.matches("Ok").count();
    let forty_two_count = output.matches("42").count();

    assert!(
        ok_count >= 2,
        "expected both subscribers to receive [Ok v]; got: {output}"
    );
    assert!(
        forty_two_count >= 2,
        "expected both subscribers to receive value 42; got: {output}"
    );
}

#[tokio::test]
async fn broadcast_channel_late_subscriber_misses_early_messages() {
    // Send a message, then subscribe — the late subscriber should not receive the early message.
    // This verifies that broadcast channels only deliver messages sent AFTER subscription.
    let source = r#"
[
  bcast: [builtin-broadcast-channel 2]
  _: [builtin-send $bcast 1]
  sub: [builtin-subscribe $bcast]
  _: [builtin-send $bcast 2]
  recv: [builtin-recv $sub]
  value: $recv.v
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // The subscriber should receive the second message (2), not the first (1).
    // DisplayVisitor format: Dict({"value": Int(2), ...}) — the field holding the received
    // value is named "value" by the LLT source (`value: $recv.v`).
    assert!(
        output.contains("Int(2)"),
        "expected late subscriber to receive second message (value=2); got: {output}"
    );
}

// ---------------------------------------------------------------------------
// select-once protocol: [Ok v] / [Closed]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn select_once_success_returns_ok_variant() {
    // Create a channel, send a value, select-once on it — should return [Ok v].
    let source = r#"
[
  ch: [builtin-channel 1]
  _: [builtin-send $ch 42]
  sources: [builtin-seq [ch: $ch  handler: [fn [let v] $v]] []]
  result: [builtin-select-once [builtin-context] $sources]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Ok".
    // DisplayVisitor format: Dict({"@": String("Ok"), ...})
    assert!(
        output.contains("Ok"),
        "expected select-once to return [Ok v]; got: {output}"
    );
    assert!(
        output.contains("42"),
        "expected select-once payload to be 42; got: {output}"
    );
}

#[tokio::test]
async fn select_once_all_closed_returns_closed_variant() {
    // Create a oneshot channel, close it, select-once on it — should return [Closed].
    let source = r#"
[
  pair: [builtin-oneshot-channel]
  sender: $pair.sender
  receiver: $pair.receiver
  _: [builtin-close-sender $sender]
  sources: [builtin-seq [ch: $receiver  handler: [fn [let v] $v]] []]
  result: [builtin-select-once [builtin-context] $sources]
  tag: $result.@
]
"#;
    let output = eval_source_with_config(source, false).expect("eval should succeed");

    // Verify the result has tag "Closed"
    assert!(
        output.contains("Closed"),
        "expected select-once on closed channels to return [Closed]; got: {output}"
    );
}
