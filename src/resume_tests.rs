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

#[tokio::test]
async fn turn_answers_in_the_context_match_on_rebuild_and_replay() {
    // The step-limit summary refers to its turn_end record, so the context
    // record covers it (eviction lists it, resume restores it); a cut-off
    // turn's partial reply is recorded but, as live, not replayed.
    let d = tmp("answers-in-context");
    let work = d.join("work");
    for n in ["a.txt", "b.txt"] {
        std::fs::write(work.join(n), "x\n").unwrap();
    }
    let p = |n: &str| work.join(n).display().to_string();
    let cut_off =
        json!({ "role": "assistant", "content": "I was about to", "finish_reason": "length" });
    let base = mock_model(vec![
        call("r1", "read_file", json!({ "path": p("a.txt") })),
        call("r2", "read_file", json!({ "path": p("b.txt") })),
        answer("Read both files; nothing changed yet."),
        cut_off,
    ])
    .await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000); // no compaction: replay must match
    cfg.agent.max_steps = 2;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Read the files.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::StepCap(Some(_)))),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    assert!(
        ctx.messages().last().unwrap().origin.is_some(),
        "summary has its record"
    );
    ctx.config.agent.max_steps = 10;
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Now explain.").await;
    assert!(
        matches!(
            &result,
            Ok(crate::agent::TurnOutcome::NoAnswer {
                truncated: true,
                ..
            })
        ),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    assert!(!expected.iter().any(|m| m["content"] == "I was about to"));
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let as_json = |ms: &[crate::context::Message]| -> Vec<Value> {
        ms.iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    };
    let r = rebuild(&t, 4096);
    assert_eq!(r.method, "context", "{:?}", r.reason);
    assert_eq!(as_json(&r.messages), expected);
    assert_eq!(as_json(&replay(&t, 4096).messages), expected);
}

/// A project whose `make test` passes only when value.txt says "fixed", and
/// prints what it found otherwise.
fn gate_project(work: &std::path::Path) {
    std::fs::write(work.join("value.txt"), "broken\n").unwrap();
    std::fs::write(
        work.join("Makefile"),
        "test:\n\t@v=$$(cat value.txt); [ \"$$v\" = fixed ] || { echo \"FAIL: value is $$v\"; exit 1; }\n",
    )
    .unwrap();
}

#[tokio::test]
async fn verifier_gate_sends_back_new_failures_then_accepts() {
    let d = tmp("gate");
    let work = d.join("work");
    gate_project(&work);
    let v = work.join("value.txt").display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": v })),
        call(
            "e1",
            "edit_file",
            json!({ "path": v, "old_string": "broken", "new_string": "wrong" }),
        ),
        answer("Done: changed the value."),
        call(
            "e2",
            "edit_file",
            json!({ "path": v, "old_string": "wrong", "new_string": "fixed" }),
        ),
        answer("Fixed: value.txt now says fixed."),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(32_768);
    cfg.security.require_approval_for_bash = false;
    cfg.security.require_approval_for_writes = false;
    cfg.verify.enabled = true;
    cfg.verify.commands = vec![format!("make -s -C {} test", work.display())];
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::Answered(a)) => assert!(a.starts_with("Fixed:"), "{a}"),
        other => panic!("expected an accepted answer: {other:?}"),
    }
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let texts: Vec<&str> = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    let fb = texts
        .iter()
        .find(|t| t.starts_with("[automatic check 1 of 3]"))
        .expect("feedback sent");
    assert!(fb.contains("FAIL: value is wrong"), "{fb}");
    assert!(
        !fb.contains("value is broken"),
        "baseline failure must not be reported: {fb}"
    );
    assert!(
        texts.contains(&"Done: changed the value."),
        "rejected answer kept in context"
    );
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    crate::schema_tests::validate_transcript(&path);
    let t = Transcript::load(&path).unwrap();
    let kinds: Vec<(String, bool)> = t
        .lines
        .iter()
        .filter_map(|l| match &l.record {
            Record::Verification(v) => Some((v.kind.clone(), v.passed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("baseline".into(), false),
            ("gate".into(), false),
            ("gate".into(), true)
        ]
    );
    // Audit record: verifier runs are rule-caused, their command approvals
    // are recorded, and the chain holds.
    let raw: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let rules: Vec<(&str, &str)> = raw
        .iter()
        .filter(|r| r["type"] == "verification")
        .map(|r| (r["actor"].as_str().unwrap(), r["rule"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rules,
        vec![
            ("rule", "verify.baseline"),
            ("rule", "verify.gate"),
            ("rule", "verify.gate")
        ]
    );
    let approvals = raw
        .iter()
        .filter(|r| {
            r["type"] == "approval"
                && r["call_id"].as_str().unwrap().starts_with("verify-")
                && r["rule"] == "approval.bash_not_required"
        })
        .count();
    assert_eq!(approvals, 3, "one approval record per verifier run");
    // Each run has its own call id, and its verification record answers
    // its approval: the approvals check has nothing to warn about.
    let ids: std::collections::BTreeSet<&str> = raw
        .iter()
        .filter(|r| r["type"] == "verification")
        .map(|r| r["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    let findings = crate::audit::verify(&path, None);
    let fails: Vec<_> = findings
        .iter()
        .filter(|f| f.level == crate::audit::Level::Fail && matches!(f.check, "lines" | "chain"))
        .collect();
    assert!(fails.is_empty(), "{fails:?}");
    let approvals: Vec<_> = findings
        .iter()
        .filter(|f| f.check == "approvals" && f.level != crate::audit::Level::Pass)
        .collect();
    assert!(approvals.is_empty(), "{approvals:?}");
    assert_resumes(&path, &expected, &live_kinds);
}

/// Makes `work` a git repository with its files committed.
fn git_repo(work: &std::path::Path) {
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-q", "-m", "start"],
    ] {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(work)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

/// A gate configuration for `gate_project` in `work`.
fn gate_config(base: String, d: &std::path::Path, work: &std::path::Path) -> crate::config::Config {
    let mut cfg = config(base, d);
    cfg.context.window = Some(32_768);
    cfg.security.require_approval_for_bash = false;
    cfg.security.require_approval_for_writes = false;
    cfg.verify.enabled = true;
    cfg.verify.commands = vec![format!("make -s -C {} test", work.display())];
    crate::agent::GATE_DIR.with(|g| *g.borrow_mut() = Some(work.to_path_buf()));
    cfg
}

#[tokio::test]
async fn verifier_gate_sees_changes_made_through_the_shell() {
    // The baseline runs before the first shell command, and a command that
    // changes files counts as a change.
    let d = tmp("gate-shell");
    let work = d.join("work");
    gate_project(&work);
    git_repo(&work);
    let v = work.join("value.txt").display().to_string();
    let script = vec![
        call(
            "b1",
            "execute_bash",
            json!({ "command": format!("printf 'wrong\\n' > {v}") }),
        ),
        answer("Done."),
        call(
            "b2",
            "execute_bash",
            json!({ "command": format!("printf 'fixed\\n' > {v}") }),
        ),
        answer("Fixed."),
    ];
    let base = mock_model(script).await;
    let cfg = gate_config(base, &d, &work);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::Answered(a)) => assert_eq!(a, "Fixed."),
        other => panic!("expected an accepted answer: {other:?}"),
    }
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let fb = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|t| t.starts_with("[automatic check 1 of 3]"))
        .expect("feedback sent");
    assert!(fb.contains("FAIL: value is wrong"), "{fb}");
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let v: Vec<(String, bool)> = verifications(&path)
        .into_iter()
        .map(|(k, _, passed, _, _)| (k, passed))
        .collect();
    assert_eq!(
        v,
        vec![
            ("baseline".into(), false),
            ("gate".into(), false),
            ("gate".into(), true)
        ]
    );
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn verifier_gate_accepts_an_answer_on_the_last_step() {
    // No step is left to repair: the answer is accepted as unverified
    // instead of being replaced by the step-limit summary.
    let d = tmp("gate-last");
    let work = d.join("work");
    gate_project(&work);
    let v = work.join("value.txt").display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": v })),
        call(
            "e1",
            "edit_file",
            json!({ "path": v, "old_string": "broken", "new_string": "wrong" }),
        ),
        answer("Done."),
    ];
    let base = mock_model(script).await;
    let mut cfg = gate_config(base, &d, &work);
    cfg.agent.max_steps = 3;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::Answered(a)) => assert_eq!(a, "Done."),
        other => panic!("expected the answer to be accepted: {other:?}"),
    }
    record_outcome(&mut ctx, &result);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let v: Vec<String> = verifications(&path).into_iter().map(|v| v.0).collect();
    assert_eq!(v, vec!["baseline", "unverified"]);
}

#[tokio::test]
async fn cut_off_reply_before_a_budget_notice_is_not_replayed() {
    // Live, a reply cut off at the reply limit is not kept (a continue
    // notice follows); a verification notice after it must not bring it
    // back on replay.
    let d = tmp("cut-off-wrap-up");
    let work = d.join("work");
    std::fs::write(work.join("a.txt"), "x\n").unwrap();
    let a = work.join("a.txt").display().to_string();
    let cut_off =
        json!({ "role": "assistant", "content": "I was about to", "finish_reason": "length" });
    let base = mock_model(vec![
        call("r1", "read_file", json!({ "path": a })),
        cut_off,
        answer("Done."),
    ])
    .await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000);
    cfg.agent.max_steps = 4;
    cfg.agent.wrap_up_percent = 50;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Read a.txt.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::Answered(_))),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let texts: Vec<&str> = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(texts.iter().any(|t| t.starts_with("[budget]")), "{texts:?}");
    assert!(!texts.contains(&"I was about to"));
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn nudge_before_gate_feedback_resumes_in_order() {
    // A loop-guard nudge goes in after its step's results, before the
    // answer the gate sends back; replay keeps that order.
    let d = tmp("gate-nudge");
    let work = d.join("work");
    gate_project(&work);
    let v = work.join("value.txt").display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": v })),
        call(
            "e1",
            "edit_file",
            json!({ "path": v, "old_string": "broken", "new_string": "wrong" }),
        ),
        call("r2", "read_file", json!({ "path": v })),
        call("r3", "read_file", json!({ "path": v })),
        answer("Done."),
        call(
            "e2",
            "edit_file",
            json!({ "path": v, "old_string": "wrong", "new_string": "fixed" }),
        ),
        answer("Fixed."),
    ];
    let base = mock_model(script).await;
    let cfg = gate_config(base, &d, &work);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::Answered(_))),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let texts: Vec<&str> = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    let at = |p: &str| texts.iter().position(|t| t.starts_with(p)).unwrap();
    assert!(at(crate::context::NUDGE) < at("Done.") && at("Done.") < at("[automatic check"));
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn verifier_gate_accepts_unverified_and_stops_after_max_rounds() {
    let d = tmp("gate-max");
    let work = d.join("work");
    gate_project(&work);
    let v = work.join("value.txt").display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": v })),
        call(
            "e1",
            "edit_file",
            json!({ "path": v, "old_string": "broken", "new_string": "wrong" }),
        ),
        answer("Done."),
        answer("Done again."),
        answer("UNVERIFIED: the test expects a value I cannot justify."),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(32_768);
    cfg.security.require_approval_for_bash = false;
    cfg.security.require_approval_for_writes = false;
    cfg.verify.enabled = true;
    cfg.verify.max_rounds = 3;
    cfg.verify.commands = vec![format!("make -s -C {} test", work.display())];
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::Answered(a)) => assert!(a.starts_with("UNVERIFIED:"), "{a}"),
        other => panic!("expected the UNVERIFIED answer to be accepted: {other:?}"),
    }
    let texts: Vec<String> = messages_json(&ctx)
        .iter()
        .filter_map(|m| m["content"].as_str().map(String::from))
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("[automatic check 2 of 3]"))
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.starts_with("[automatic check 3 of 3]"))
    );
}

#[tokio::test]
async fn syntax_guard_reverts_an_edit_that_breaks_json() {
    let d = tmp("syntax");
    let work = d.join("work");
    let f = work.join("cfg.json");
    std::fs::write(&f, "{\"a\": 1}\n").unwrap();
    let p = f.display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": p })),
        call(
            "e1",
            "edit_file",
            json!({ "path": p, "old_string": "1}", "new_string": "1,}" }),
        ),
        answer("done"),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(32_768);
    cfg.security.require_approval_for_writes = false;
    cfg.verify.syntax_guard = true;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    let mut presenter = CliPresenter::default();
    run_turn(&mut ctx, &registry, &mut presenter, "Edit cfg.json.")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "{\"a\": 1}\n",
        "edit reverted"
    );
    let reverted = messages_json(&ctx)
        .iter()
        .filter_map(|m| m["content"].as_str().map(String::from))
        .any(|t| t.starts_with("Error: edit reverted") && t.contains("invalid JSON"));
    assert!(reverted);
}

#[tokio::test]
async fn syntax_guard_keeps_a_new_file_that_does_not_parse() {
    // A new file may be meant not to parse (a parser test's fixture): it is
    // kept, and the parser output is added to the result.
    let d = tmp("syntax-new");
    let work = d.join("work");
    let f = work.join("fixtures/bad.json");
    let p = f.display().to_string();
    let script = vec![
        call(
            "w1",
            "write_file",
            json!({ "path": p, "content": "{\"a\": }\n" }),
        ),
        answer("done"),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(32_768);
    cfg.security.require_approval_for_writes = false;
    cfg.verify.syntax_guard = true;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    run_turn(&mut ctx, &registry, &mut presenter, "Add a bad fixture.")
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "{\"a\": }\n");
    let noted = messages_json(&ctx)
        .iter()
        .filter_map(|m| m["content"].as_str().map(String::from))
        .any(|t| {
            t.contains("does not parse; it was written as given") && t.contains("invalid JSON")
        });
    assert!(noted);
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let v: Vec<(String, String)> = verifications(&path)
        .into_iter()
        .map(|(k, _, _, _, rule)| (k, rule))
        .collect();
    assert_eq!(
        v,
        vec![("syntax_note".into(), "verify.syntax_guard".into())]
    );
}

/// Message kinds (operator request, notice, record), which serialization
/// leaves out: rebuilt and replayed contexts must match them too.
fn kinds(ms: &[crate::context::Message]) -> Vec<String> {
    ms.iter().map(|m| format!("{:?}", m.kind)).collect()
}

/// Rebuild and replay give the live context, messages and kinds.
fn assert_resumes(path: &std::path::Path, expected: &[Value], live_kinds: &[String]) {
    crate::schema_tests::validate_transcript(path);
    let t = Transcript::load(path).unwrap();
    for got in [rebuild(&t, 4096).messages, replay(&t, 4096).messages] {
        let json: Vec<Value> = got
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(json, expected);
        assert_eq!(kinds(&got), live_kinds);
    }
}

/// The verification records of a transcript: (kind, command, passed, has
/// a message, rule).
fn verifications(path: &std::path::Path) -> Vec<(String, String, bool, bool, String)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|r| r["type"] == "verification")
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_string(),
                r["command"].as_str().unwrap().to_string(),
                r["passed"].as_bool().unwrap(),
                r["message"].is_string(),
                r["rule"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn end_check_sends_a_repair_notice_when_steps_run_low() {
    // 8 steps, the last 3 kept for repair: at step 5 the check fails, so the
    // model is told, fixes the value, and the turn ends at the limit with a
    // passing check recorded.
    let d = tmp("repair");
    let work = d.join("work");
    gate_project(&work);
    for n in ["a", "b", "c", "e", "f"] {
        std::fs::write(work.join(format!("{n}.txt")), "x\n").unwrap();
    }
    let v = work.join("value.txt").display().to_string();
    let r = |id: &str, n: &str| {
        call(
            id,
            "read_file",
            json!({ "path": work.join(format!("{n}.txt")).display().to_string() }),
        )
    };
    let script = vec![
        call("r0", "read_file", json!({ "path": v })),
        call(
            "e1",
            "edit_file",
            json!({ "path": v, "old_string": "broken", "new_string": "wrong" }),
        ),
        r("r2", "a"),
        r("r3", "b"),
        r("r4", "c"),
        call(
            "e5",
            "edit_file",
            json!({ "path": v, "old_string": "wrong", "new_string": "fixed" }),
        ),
        r("r6", "e"),
        r("r7", "f"),
        answer("Summary: value fixed."),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000);
    cfg.agent.max_steps = 8;
    cfg.security.require_approval_for_bash = false;
    cfg.security.require_approval_for_writes = false;
    cfg.verify.enabled = true;
    cfg.verify.end_check = true;
    cfg.verify.repair_steps = 3;
    cfg.verify.commands = vec![format!("make -s -C {} test", work.display())];
    crate::agent::GATE_DIR.with(|g| *g.borrow_mut() = Some(work.clone()));
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Fix value.txt.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::StepCap(Some(s))) if s.starts_with("Summary:")),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let notice = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|t| t.starts_with("[automatic check] 3 steps are left in this task."))
        .expect("repair notice");
    assert!(notice.contains("FAIL: value is wrong"), "{notice}");
    // The notice comes before step 5's edit (here right after the step
    // limit warning, which falls on the same step).
    let at = expected
        .iter()
        .position(|m| m["content"].as_str() == Some(notice))
        .unwrap();
    assert_eq!(expected[at + 1]["tool_calls"][0]["id"], "e5");
    assert_eq!(expected[at - 1]["content"], crate::context::STEPS_LEFT);
    assert_eq!(
        std::fs::read_to_string(work.join("value.txt")).unwrap(),
        "fixed\n"
    );
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let v: Vec<(String, bool, bool, String)> = verifications(&path)
        .into_iter()
        .map(|(k, _, p, m, r)| (k, p, m, r))
        .collect();
    assert_eq!(
        v,
        vec![
            ("baseline".into(), false, false, "verify.baseline".into()),
            ("limit".into(), false, true, "verify.limit".into()),
            ("end".into(), true, false, "verify.limit".into()),
        ]
    );
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn end_check_names_changed_tests_and_new_files_once() {
    // The checks pass, but the turn edited an existing test file and left a
    // new file: the answer is held back once with those facts, and the
    // next answer is accepted.
    let d = tmp("facts");
    let work = d.join("work");
    std::fs::create_dir_all(work.join("tests")).unwrap();
    // The check writes a file of its own; that is not the task's.
    std::fs::write(work.join("Makefile"), "test:\n\t@echo run > cov.out\n").unwrap();
    std::fs::write(work.join("tests/check.sh"), "exit 0\n").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(&work)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "start"]);
    let t = work.join("tests/check.sh").display().to_string();
    let n = work.join("scratch.txt").display().to_string();
    let script = vec![
        call("r1", "read_file", json!({ "path": t })),
        call(
            "e1",
            "edit_file",
            json!({ "path": t, "old_string": "exit 0", "new_string": "exit 0 # ok" }),
        ),
        call(
            "w1",
            "write_file",
            json!({ "path": n, "content": "notes\n" }),
        ),
        answer("Done."),
        answer("Done; both changes are intended."),
    ];
    let base = mock_model(script).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000);
    cfg.security.require_approval_for_bash = false;
    cfg.security.require_approval_for_writes = false;
    cfg.verify.enabled = true;
    cfg.verify.end_check = true;
    cfg.verify.commands = vec![format!("make -s -C {} test", work.display())];
    crate::agent::GATE_DIR.with(|g| *g.borrow_mut() = Some(work.clone()));
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Tidy the test.").await;
    match &result {
        Ok(crate::agent::TurnOutcome::Answered(a)) => assert!(a.starts_with("Done; both"), "{a}"),
        other => panic!("expected the second answer to be accepted: {other:?}"),
    }
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let fb = expected
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|t| t.starts_with("[automatic check 1 of 3] The checks pass."))
        .expect("facts sent");
    assert!(
        fb.contains("Existing test files changed in this task: tests/check.sh."),
        "{fb}"
    );
    assert!(
        fb.contains("New files not tracked by git: scratch.txt."),
        "{fb}"
    );
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let v: Vec<(String, String, bool)> = verifications(&path)
        .into_iter()
        .map(|(k, c, _, m, _)| (k, if c.is_empty() { "" } else { "cmd" }.to_string(), m))
        .collect();
    assert_eq!(
        v,
        vec![
            ("baseline".into(), "cmd".into(), false),
            ("gate".into(), "cmd".into(), false), // first answer: passes
            ("gate".into(), "".into(), true),     // ... and the facts
            ("gate".into(), "cmd".into(), false), // second answer: passes
        ]
    );
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn time_limit_ends_the_turn_with_a_summary() {
    let d = tmp("time-limit");
    let base = mock_model(vec![answer("Summary: no time to start.")]).await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000);
    cfg.agent.max_seconds = Some(0);
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Do something.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::TimeLimit(Some(s))) if s.starts_with("Summary:")),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    assert!(
        expected
            .iter()
            .any(|m| m["content"].as_str() == Some(crate::context::TIME_SUMMARY))
    );
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    let t = Transcript::load(&path).unwrap();
    let outcome = t.lines.iter().rev().find_map(|l| match &l.record {
        Record::TurnEnd(e) => Some(e.outcome.clone()),
        _ => None,
    });
    assert_eq!(outcome.as_deref(), Some("time_limit"));
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains(r#""rule":"session.time_limit_summary""#));
    assert_resumes(&path, &expected, &live_kinds);
}

#[tokio::test]
async fn wrap_up_notice_reports_the_budget() {
    // At 50% of 6 steps, a budget notice; nothing changed, so no check runs.
    let d = tmp("wrap-up");
    let work = d.join("work");
    for n in ["a", "b", "c"] {
        std::fs::write(work.join(format!("{n}.txt")), "x\n").unwrap();
    }
    let r = |id: &str, n: &str| {
        call(
            id,
            "read_file",
            json!({ "path": work.join(format!("{n}.txt")).display().to_string() }),
        )
    };
    let base = mock_model(vec![
        r("r0", "a"),
        r("r1", "b"),
        r("r2", "c"),
        answer("All three say x."),
    ])
    .await;
    let mut cfg = config(base, &d);
    cfg.context.window = Some(200_000);
    cfg.agent.max_steps = 6;
    cfg.agent.wrap_up_percent = 50;
    let registry = ToolRegistry::init_default(&cfg);
    let mut ctx = AgentContext::new(cfg, registry.specs());
    ctx.session.enable(json!({ "mode": "test" })).unwrap();
    let mut presenter = CliPresenter::default();
    let result = run_turn(&mut ctx, &registry, &mut presenter, "Read the files.").await;
    assert!(
        matches!(&result, Ok(crate::agent::TurnOutcome::Answered(_))),
        "{result:?}"
    );
    record_outcome(&mut ctx, &result);
    let expected = messages_json(&ctx);
    let live_kinds = kinds(&ctx.messages()[1..]);
    let at = expected
        .iter()
        .position(|m| {
            m["content"].as_str()
                == Some(
                    "[budget] Step 3 of 6.\nThe task ends at the limit; finish and check the \
                     work within it.",
                )
        })
        .expect("budget notice");
    assert_eq!(expected[at - 1]["tool_call_id"], "r2");
    let path = ctx.session.transcript_path().unwrap().to_path_buf();
    drop(ctx);
    assert_eq!(
        verifications(&path),
        vec![(
            "wrap_up".into(),
            "".into(),
            false, // no command ran
            true,
            "verify.wrap_up".into()
        )]
    );
    assert_resumes(&path, &expected, &live_kinds);
}
