//! End-to-end resume tests: run the real agent loop against a scripted model,
//! then rebuild the context from the transcript it wrote.

use serde_json::{Value, json};
use std::path::PathBuf;

use crate::agent::{record_outcome, run_turn};
use crate::config::Config;
use crate::context::AgentContext;
use crate::presenter::CliPresenter;
use crate::testutil::{answer, call, mock_model};
use crate::tools::ToolRegistry;
use crate::transcript::{Record, Transcript, rebuild, replay};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mima-resume-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("work")).unwrap();
    d
}

fn config(base_url: String, dir: &std::path::Path) -> Config {
    let mut c = Config::default();
    c.provider.base_url = base_url;
    c.provider.default_model = "mock".into();
    c.agent.stream = false;
    c.agent.max_tokens = 100;
    // A fixed prompt keeps the budget, and so whether masking happens,
    // independent of the default prompt's length.
    c.agent.system_prompt_override = "You are a coding agent under test.".into();
    c.context.window = Some(4_400); // small, so outputs are capped and masked
    c.context.server_tokenize = false;
    c.security.allowed_paths = vec![dir.join("work").display().to_string()];
    c.session.transcript_dir = dir.join("tx").display().to_string();
    c
}

fn messages_json(ctx: &AgentContext) -> Vec<Value> {
    ctx.messages()[1..]
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect()
}

/// The pairing invariant: every tool result follows its request.
fn assert_pairing(messages: &[crate::context::Message]) {
    let mut open: Vec<String> = Vec::new();
    for m in messages {
        if m.role == "tool" {
            let id = m.tool_call_id.clone().unwrap();
            assert!(open.contains(&id), "orphan result {id}");
            open.retain(|o| *o != id);
        } else {
            assert!(open.is_empty(), "unanswered {open:?}");
            if let Some(calls) = m.tool_calls.as_ref().and_then(Value::as_array) {
                open = calls
                    .iter()
                    .map(|c| c["id"].as_str().unwrap().to_string())
                    .collect();
            }
        }
    }
}

#[tokio::test]
async fn context_record_restores_exactly_what_the_model_saw() {
    let d = tmp("exact");
    let work = d.join("work");
    let big: String = (1..=600)
        .map(|i| format!("line {i} of a long file\n"))
        .collect();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(work.join(name), &big).unwrap();
    }
    let p = |n: &str| work.join(n).display().to_string();
    let base = mock_model(vec![
        call("r1", "read_file", json!({ "path": p("a.txt") })),
        call("r2", "read_file", json!({ "path": p("b.txt") })),
        call("r3", "read_file", json!({ "path": p("c.txt") })),
        call(
            "e1",
            "edit_file",
            json!({ "path": p("a.txt"), "old_string": "line 7 of a long file",
                                        "new_string": "line seven" }),
        ),
        answer("Edited a.txt."),
        call(
            "r4",
            "read_file",
            json!({ "path": p("a.txt"), "offset": 5, "limit": 5 }),
        ),
        answer("Line 7 now reads: line seven."),
    ])
    .await;

    let cfg = config(base, &d);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session
        .enable(json!({ "mode": "test", "cwd": d.display().to_string() }))
        .unwrap();
    let mut presenter = CliPresenter::default();
    ctx.config.security.approve_all = true;
    for instruction in [
        "Read the files and fix line 7 of a.txt.",
        "What does line 7 say now?",
    ] {
        let result = run_turn(&mut ctx, &registry, &mut presenter, instruction).await;
        assert!(result.is_ok(), "{:?}", result.err());
        record_outcome(&mut ctx, &result);
    }
    assert!(
        ctx.report().masked_total > 0,
        "the test should exercise masking"
    );
    let expected = messages_json(&ctx);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx); // releases the transcript lock

    // Everything the real writers produced matches the schema.
    crate::schema_tests::validate_transcript(&path);

    let t = Transcript::load(&path).unwrap();
    assert_eq!(t.bad_lines, 0);
    let contexts = t
        .lines
        .iter()
        .filter(|l| matches!(l.record, Record::Context(_)))
        .count();
    assert!(contexts >= 2, "turn-end context records written");

    // Rebuilding from the last context record gives the exact messages.
    let r = rebuild(&t, 4096);
    assert_eq!(r.method, "context", "{:?}", r.reason);
    let got: Vec<Value> = r
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(got, expected);
    assert!(r.messages.iter().any(|m| m.masked));

    // Replay gives the same conversation (before compaction), correctly paired.
    let replayed = replay(&t, 4096);
    assert_pairing(&replayed.messages);
    assert_eq!(
        replayed.messages.first().unwrap().content.as_deref(),
        Some("Read the files and fix line 7 of a.txt.")
    );
    assert_eq!(
        replayed.messages.last().unwrap().content.as_deref(),
        Some("Line 7 now reads: line seven.")
    );

    // The file the session edited is recorded; editing it again is noticed.
    assert!(crate::transcript::changed_files(&t, &d).is_empty());
    std::fs::write(work.join("a.txt"), "changed by someone else\n").unwrap();
    let changed = crate::transcript::changed_files(&t, &d);
    assert_eq!(changed.len(), 1);
    assert!(changed[0].path.ends_with("a.txt") && changed[0].status == "changed");

    // Save as the golden fixture for this schema version (MIMA_BLESS=1).
    if std::env::var("MIMA_BLESS").is_ok_and(|v| v == "1") {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&path, dir.join("edit-and-ask.jsonl")).unwrap();
    }
}

/// Golden fixtures for the current transcript and context schema versions.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "testdata/transcripts/s{}-c{}",
        crate::session::SCHEMA,
        crate::transcript::CONTEXT_SCHEMA
    ))
}

#[tokio::test]
async fn compaction_record_is_restored_and_replayed() {
    let d = tmp("record");
    let work = d.join("work");
    let big: String = (1..=600)
        .map(|i| format!("line {i} of a long file\n"))
        .collect();
    std::fs::write(work.join("a.txt"), &big).unwrap();
    let p = work.join("a.txt").display().to_string();
    let mut script: Vec<Value> = (0..24)
        .map(|i| {
            call(
                &format!("r{i}"),
                "read_file",
                json!({ "path": p, "offset": 1 + i * 20, "limit": 20 }),
            )
        })
        .collect();
    script.push(answer("Read all of a.txt."));
    script.push(answer("It has 600 lines."));
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.agent.max_steps = 40;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    for instruction in ["Read a.txt in pieces.", "How long is it?"] {
        let result = run_turn(&mut ctx, &registry, &mut presenter, instruction).await;
        assert!(result.is_ok(), "{:?}", result.err());
        record_outcome(&mut ctx, &result);
    }
    let records = ctx.report().records;
    assert!(records > 0, "the test should exercise eviction");
    let expected = messages_json(&ctx);
    let record_text = |m: &crate::context::Message| {
        (m.kind == crate::context::Kind::Record).then(|| m.content.clone().unwrap_or_default())
    };
    let live: Vec<String> = ctx.messages().iter().filter_map(record_text).collect();
    assert_eq!(live.len(), 1);
    assert!(live[0].contains("a.txt (lines 1-"), "{}", live[0]);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);

    let t = Transcript::load(&path).unwrap();
    let r = rebuild(&t, 4096);
    assert_eq!(r.method, "context", "{:?}", r.reason);
    let got: Vec<Value> = r
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(got, expected);
    assert_eq!(r.records, records);
    assert!(r.digest.calls > 0);
    let rebuilt: Vec<String> = r.messages.iter().filter_map(record_text).collect();
    assert_eq!(rebuilt, live, "the record comes back as a record");

    // Replay applies the record too: evicted steps stay out, pairing holds.
    let replayed = replay(&t, 4096);
    assert_pairing(&replayed.messages);
    let replayed_records: Vec<String> = replayed.messages.iter().filter_map(record_text).collect();
    assert_eq!(replayed_records, live);
    assert_eq!(replayed.records, records);
    assert!(
        !replayed
            .messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("r0")),
        "evicted results are not replayed"
    );

    if std::env::var("MIMA_BLESS").is_ok_and(|v| v == "1") {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&path, dir.join("compaction-record.jsonl")).unwrap();
    }
}

#[tokio::test]
async fn resumed_session_continues_in_the_same_file() {
    let d = tmp("continue");
    let base = mock_model(vec![answer("first answer"), answer("second answer")]).await;
    let cfg = config(base.clone(), &d);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    ctx.config.security.approve_all = true;
    let result = run_turn(&mut ctx, &registry, &mut presenter, "first question").await;
    record_outcome(&mut ctx, &result);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    let id = ctx.session.id().to_string();
    drop(ctx);

    // A new process: rebuild and continue appending to the same transcript.
    let t = Transcript::load(&path).unwrap();
    let r = rebuild(&t, 4096);
    assert_eq!(r.method, "context");
    let cfg = config(base, &d);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session = crate::session::Session::resume(
        &path,
        1 << 20,
        t.start().unwrap().session_started.clone(),
        t.next_seq(),
        t.turns(),
    )
    .unwrap();
    ctx.restore(
        r.messages,
        r.first_instruction,
        r.masked_total,
        r.evicted_total,
    );
    assert_eq!(ctx.session.id(), id);
    let result = run_turn(&mut ctx, &registry, &mut presenter, "second question").await;
    record_outcome(&mut ctx, &result);
    let contents: Vec<String> = ctx.messages()[1..]
        .iter()
        .filter_map(|m| m.content.clone())
        .collect();
    assert_eq!(
        contents,
        [
            "first question",
            "first answer",
            "second question",
            "second answer"
        ]
    );
    drop(ctx);

    // One file, increasing seq, turn numbers continue.
    let t = Transcript::load(&path).unwrap();
    let seqs: Vec<u64> = t.lines.iter().map(|l| l.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    assert_eq!(t.turns(), 2);
    crate::schema_tests::validate_transcript(&path);
}

#[tokio::test]
async fn empty_reply_gets_a_continue_prompt_and_resumes_exactly() {
    let d = tmp("empty-reply");
    let base = mock_model(vec![answer(""), answer("Done.")]).await;
    let cfg = config(base, &d);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Say done.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::Answered(a)) if a == "Done."),
        "an empty reply must not end the turn: {result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    assert!(
        expected
            .iter()
            .any(|m| m["content"] == crate::context::CONTINUE),
        "the continue prompt is in the conversation"
    );
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let r = rebuild(&t, 4096);
    let got: Vec<Value> = r
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(got, expected);
    let replayed = replay(&t, 4096);
    assert!(
        replayed
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some(crate::context::CONTINUE))
    );
}

#[tokio::test]
async fn step_limit_warns_then_asks_for_a_summary() {
    let d = tmp("step-cap");
    let work = d.join("work");
    std::fs::write(work.join("a.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n").unwrap();
    let p = work.join("a.txt").display().to_string();
    let mut script: Vec<Value> = (1..=8)
        .map(|i| {
            call(
                &format!("r{i}"),
                "read_file",
                json!({ "path": p, "offset": i, "limit": 1 }),
            )
        })
        .collect();
    script.push(answer("Summary: read the file; nothing changed."));
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.agent.max_steps = 8;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Look around.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::StepCap(Some(s))) => assert!(s.starts_with("Summary:")),
        other => panic!("expected a step cap with a summary: {other:?}"),
    }
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let texts: Vec<&str> = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(texts.contains(&crate::context::STEPS_LEFT));
    assert!(texts.contains(&crate::context::FINAL_SUMMARY));
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let got: Vec<Value> = rebuild(&t, 4096)
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn replies_that_stay_empty_end_without_an_answer() {
    // Pro-task calibration: models that reasoned until the reply limit, three
    // times, used to end the turn as "answered" with an empty answer.
    let d = tmp("no-answer");
    let base = mock_model(vec![answer(""), answer(""), answer("")]).await;
    let cfg = config(base, &d);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Say done.").await;
    assert!(
        matches!(
            &result,
            Ok(crate::agent::TurnOutcome::NoAnswer {
                truncated: false,
                ..
            })
        ),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let outcome = t.lines.iter().rev().find_map(|l| match &l.record {
        crate::transcript::Record::TurnEnd(e) => Some(e.outcome.clone()),
        _ => None,
    });
    assert_eq!(outcome.as_deref(), Some("no_answer"));
}

#[tokio::test]
async fn arguments_are_converted_or_refused_and_recorded() {
    // A string line number is converted and recorded; one that is not a
    // number is refused with the reason instead of reading from line 1.
    let d = tmp("convert-args");
    let work = d.join("work");
    std::fs::write(work.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let a = work.join("a.txt").display().to_string();
    let base = mock_model(vec![
        call(
            "r1",
            "read_file",
            json!({ "path": a, "offset": "2", "limit": null }),
        ),
        call("r2", "read_file", json!({ "path": a, "offset": "two" })),
        answer("done"),
    ])
    .await;
    let cfg = config(base, &d);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Read a.txt.").await;
    record_outcome(&mut ctx, &result);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let results: Vec<_> = t
        .lines
        .iter()
        .filter_map(|l| match &l.record {
            Record::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(!results[0].failed, "{}", results[0].output);
    let shown = &results[0].output;
    assert!(
        shown.contains("     2\ttwo") && !shown.contains("one"),
        "{shown}"
    );
    assert_eq!(
        results[0].converted,
        Some(vec![
            json!({ "arg": "limit", "from": null }),
            json!({ "arg": "offset", "from": "2" })
        ])
    );
    assert!(results[1].failed);
    assert!(
        results[1]
            .output
            .contains("read_file: argument `offset` must be a whole number, got \"two\""),
        "{}",
        results[1].output
    );
    assert!(results[1].converted.is_none());
}
