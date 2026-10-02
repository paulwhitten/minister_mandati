//! Schema and compatibility checks for transcripts
//! (docs/design/session-resume.md, "Checks"). `MIMA_BLESS=1 cargo test`
//! rewrites the committed schema and golden fixtures.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::transcript::{CONTEXT_SCHEMA, Line, Transcript, rebuild};

const SCHEMA_FILE: &str = "schemas/transcript.schema.json";
/// The schema of the last release; breaking changes against it need a
/// version bump. Refresh it (copy the current schema) at each release.
const RELEASED_FILE: &str = "schemas/released/transcript.schema.json";
const FIXTURES: &str = "testdata/transcripts";

fn bless() -> bool {
    std::env::var("MIMA_BLESS").is_ok_and(|v| v == "1")
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The schema derived from the record types, with mima's extensions.
pub fn generated() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(Line)).unwrap();
    v["x-mima-versions"] =
        json!({ "schema": crate::session::SCHEMA, "context_schema": CONTEXT_SCHEMA });
    // Closed enums are needed to rebuild a context; the other enum-like
    // fields are plain strings (open: unknown values are informational).
    v["x-mima-enum-kind"] = json!({ "View": "closed" });
    // Names that were removed and may never be reused (evolution rule 6).
    v["x-mima-retired"] = json!([]);
    v
}

/// "Closed for writers": the committed schema with every undocumented
/// property rejected, used to validate what mima writes.
fn closed(schema: &Value) -> Value {
    let mut s = schema.clone();
    s["unevaluatedProperties"] = json!(false);
    for name in [
        "Approvals",
        "CallRecord",
        "Usage",
        "Tokens",
        "FileState",
        "FileChange",
        "Entry",
        "Check",
    ] {
        if let Some(def) = s["$defs"].get_mut(name) {
            def["additionalProperties"] = json!(false);
        }
    }
    s
}

/// Validates every line of a transcript against the closed schema.
pub fn validate_transcript(path: &Path) {
    let schema = closed(&generated());
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let text = std::fs::read_to_string(path).unwrap();
    for (i, line) in text.lines().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap();
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(
            errors.is_empty(),
            "line {i} ({}) invalid: {errors:?}",
            v["type"]
        );
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap() + "\n"
}

// C1
#[test]
fn committed_schema_is_current() {
    let path = root().join(SCHEMA_FILE);
    let want = pretty(&generated());
    if bless() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "{SCHEMA_FILE} is out of date with the record types; run `MIMA_BLESS=1 cargo test` and review the diff"
    );
}

// C3 + C6
#[test]
fn golden_fixtures_parse_rebuild_and_round_trip() {
    let dir = root().join(FIXTURES);
    let mut seen = 0;
    for version in std::fs::read_dir(&dir).expect("fixtures directory") {
        let version = version.unwrap().path();
        for f in std::fs::read_dir(&version).unwrap() {
            let f = f.unwrap().path();
            if f.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            seen += 1;
            let text = std::fs::read_to_string(&f).unwrap();
            for line in text.lines() {
                // Round trip is stable, and an unknown key changes nothing.
                let parsed: Line = serde_json::from_str(line).unwrap();
                let again: Line =
                    serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
                assert_eq!(
                    serde_json::to_value(&parsed).unwrap(),
                    serde_json::to_value(&again).unwrap()
                );
                let mut extra: Value = serde_json::from_str(line).unwrap();
                extra["zz_future_field"] = json!({ "x": 1 });
                let with_extra: Line = serde_json::from_value(extra).unwrap();
                assert_eq!(
                    serde_json::to_value(&parsed).unwrap(),
                    serde_json::to_value(&with_extra).unwrap()
                );
            }
            // The latest context rebuilds to the recorded message list.
            let t = Transcript::load(&f).unwrap();
            let r = rebuild(&t, 4096);
            let got: Vec<Value> = r
                .messages
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect();
            let expected_path = f.with_extension("expected.json");
            if bless() {
                std::fs::write(
                    &expected_path,
                    pretty(&json!({ "method": r.method, "messages": got })),
                )
                .unwrap();
            }
            let expected: Value =
                serde_json::from_str(&std::fs::read_to_string(&expected_path).unwrap()).unwrap();
            assert_eq!(expected["method"], json!(r.method), "{}", f.display());
            assert_eq!(expected["messages"], json!(got), "{}", f.display());
        }
    }
    assert!(seen > 0, "no golden fixtures found");
}

// C5: breaking changes against the released schema need a version bump.
#[test]
fn schema_changes_follow_the_evolution_rules() {
    let released_path = root().join(RELEASED_FILE);
    let Ok(text) = std::fs::read_to_string(&released_path) else {
        panic!("{RELEASED_FILE} missing: copy {SCHEMA_FILE} there at each release");
    };
    let old: Value = serde_json::from_str(&text).unwrap();
    let new = generated();
    let bumped = |key: &str| old["x-mima-versions"][key] != new["x-mima-versions"][key];
    let problems = breaking_changes(&old, &new);
    if !problems.is_empty() {
        assert!(
            bumped("schema") || bumped("context_schema"),
            "breaking schema changes without a version bump:\n{}",
            problems.join("\n")
        );
    }
}

/// Differences that break readers (rules 3-8), by definition name.
fn breaking_changes(old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let empty = serde_json::Map::new();
    let old_defs = old["$defs"].as_object().unwrap_or(&empty);
    let new_defs = new["$defs"].as_object().unwrap_or(&empty);
    let retired: Vec<&str> = new["x-mima-retired"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for (name, o) in old_defs {
        let Some(n) = new_defs.get(name) else {
            out.push(format!("{name}: definition removed"));
            continue;
        };
        let props = |d: &Value| d["properties"].as_object().cloned().unwrap_or_default();
        let (op, np) = (props(o), props(n));
        for (field, ot) in &op {
            match np.get(field) {
                None => out.push(format!("{name}.{field}: removed")),
                Some(nt)
                    if ot.get("type") != nt.get("type") || ot.get("$ref") != nt.get("$ref") =>
                {
                    out.push(format!("{name}.{field}: type changed"))
                }
                _ => {}
            }
        }
        for field in np.keys() {
            if !op.contains_key(field) && retired.contains(&field.as_str()) {
                out.push(format!("{name}.{field}: reuses a retired name"));
            }
        }
        let req = |d: &Value| {
            let mut v: Vec<String> = d["required"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        };
        if req(o) != req(n) {
            out.push(format!(
                "{name}: required fields changed ({:?} -> {:?})",
                req(o),
                req(n)
            ));
        }
        let kind = new["x-mima-enum-kind"][name].as_str();
        let values = |d: &Value| d["enum"].as_array().cloned().unwrap_or_default();
        let (ov, nv) = (values(o), values(n));
        if !ov.is_empty() {
            let removed = ov.iter().any(|v| !nv.contains(v));
            let added = nv.iter().any(|v| !ov.contains(v));
            if removed || (kind == Some("closed") && added) {
                out.push(format!("{name}: enum values changed ({ov:?} -> {nv:?})"));
            }
        }
    }
    out
}

#[test]
fn rule_checker_flags_breaking_changes() {
    let old = json!({
        "$defs": {
            "A": { "properties": { "x": { "type": "integer" }, "y": { "type": "string" } }, "required": ["x"] },
            "View": { "enum": ["full", "masked"] }
        },
        "x-mima-enum-kind": { "View": "closed" }
    });
    // Adding an optional field is fine.
    let mut ok = old.clone();
    ok["$defs"]["A"]["properties"]["z"] = json!({ "type": "string" });
    assert!(breaking_changes(&old, &ok).is_empty());
    // Removing, retyping, changing required, adding to a closed enum: breaking.
    let mut bad = old.clone();
    bad["$defs"]["A"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("y");
    bad["$defs"]["A"]["properties"]["x"] = json!({ "type": "string" });
    bad["$defs"]["A"]["required"] = json!(["x", "z"]);
    bad["$defs"]["View"]["enum"] = json!(["full", "masked", "summarized"]);
    let problems = breaking_changes(&old, &bad);
    assert_eq!(problems.len(), 4, "{problems:?}");
}
