//! Drive upstream agentproc `scenarios.json` (wire 0.4) through our pure
//! [`wire_assemble`](crate::bridge::wire_assemble) assembler.
//!
//! Fixture copied from `~/projects/agentproc/spec/conformance/scenarios.json`
//! into `tests/fixtures/agentproc_scenarios_0.4.json`.

use ilink_hub::bridge::wire_assemble::{assemble_lines, WireAssembleConfig};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    lines: Vec<String>,
    #[serde(default = "default_true")]
    streaming: bool,
    #[serde(default)]
    profile_overrides: Option<ProfileOverrides>,
    expect: Expect,
}

#[derive(Debug, Deserialize, Default)]
struct ProfileOverrides {
    max_reply_chars: Option<usize>,
    truncation_suffix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Expect {
    reply: String,
    session_id: String,
    error: String,
    exit_code: i32,
    #[serde(default)]
    partials: Vec<String>,
    /// Spec leaves boundary-chunk behaviour implementation-defined.
    #[serde(default)]
    partials_any_of: Option<Vec<Vec<String>>>,
}

fn default_true() -> bool {
    true
}

#[test]
fn agentproc_scenarios_0_4_conformance() {
    let raw = include_str!("fixtures/agentproc_scenarios_0.4.json");
    let fixture: Fixture = serde_json::from_str(raw).expect("parse scenarios fixture");
    assert!(!fixture.scenarios.is_empty());

    for scenario in &fixture.scenarios {
        let overrides = scenario.profile_overrides.as_ref();
        let cfg = WireAssembleConfig {
            streaming: scenario.streaming,
            max_reply_chars: overrides.and_then(|o| o.max_reply_chars).unwrap_or(8000),
            truncation_suffix: overrides
                .and_then(|o| o.truncation_suffix.clone())
                .unwrap_or_else(|| "\n\n…(truncated)".into()),
        };
        let out = assemble_lines(&scenario.lines, &cfg);

        assert_eq!(
            out.reply, scenario.expect.reply,
            "{}: reply mismatch",
            scenario.name
        );
        let got_sid = out.session_id.unwrap_or_default();
        assert_eq!(
            got_sid, scenario.expect.session_id,
            "{}: session_id mismatch",
            scenario.name
        );
        let got_err = out.error.unwrap_or_default();
        assert_eq!(
            got_err, scenario.expect.error,
            "{}: error mismatch",
            scenario.name
        );
        assert_eq!(
            out.exit_code, scenario.expect.exit_code,
            "{}: exit_code mismatch",
            scenario.name
        );

        if let Some(any_of) = &scenario.expect.partials_any_of {
            assert!(
                any_of.iter().any(|candidate| candidate == &out.partials),
                "{}: partials {:?} not in any_of {:?}",
                scenario.name,
                out.partials,
                any_of
            );
        } else {
            assert_eq!(
                out.partials, scenario.expect.partials,
                "{}: partials mismatch",
                scenario.name
            );
        }
    }
}
