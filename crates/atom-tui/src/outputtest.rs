//! outputtest.rs implements atom --output-test: a TUI development mode
//! that replays a canned session covering every block type and live
//! state, without a server, a model, or an API key.

use crate::app::{App, RunOptions};
use crate::blocks::{Block, BlockKind};
use crate::events::parse_stream_event;
use std::time::Duration;

/// outputTestPrompt is the canned user message starting the demo.
pub const OUTPUT_TEST_PROMPT: &str = "Add an --output-test flag to atom that replays a canned session covering every block type and live state, so the TUI can be developed without a server or an API key.";

/// outputTestTranscript is the scripted NDJSON event stream the demo
/// replays through the real stream-event handlers.
pub const OUTPUT_TEST_TRANSCRIPT: &[&str] = &[
    r#"{"type":"round_start"}"#,
    r#"{"type":"reasoning","text":"The TUI renders blocks locally, so an --output-test mode does not need the server at all: build a tuiModel with a scripted transcript and run the same Bubble Tea program. The key insight is that the live states — Working, Thinking, Compacting — are driven by the streaming flag and the spinner tick chain, so the demo should replay events through handleStreamMsg exactly like a real session would."}"#,
    r#"{"type":"reasoning","text":"Scene order matters: show the full static view first so every block type is inspectable, then cycle Working, Thinking, and Compacting, then settle back on the full view. Each scene rebuilds its blocks from a transcript prefix, which keeps the demo honest about what the real flow renders."}"#,
    r#"{"type":"reasoning_end","duration_ms":"3200"}"#,
    r#"{"type":"content","text":"I'll add --output-test to the flag set and short-circuit straight into a dedicated TUI entry point, skipping the server and model resolution entirely. The demo session replays a scripted event transcript through the same handlers a live conversation uses, so what you see is exactly what a real turn renders.\n\nThe scene cycle walks the live states in the order they appear in a real turn: Working while the request is in flight, Thinking while reasoning streams, and Compacting during a fold. After the cycle the view settles on the full history so you can scroll, toggle /thinking, and click tool blocks to expand long outputs.\n\nDocs: https://github.com/charmbracelet/bubbletea — this text renders links inline."}"#,
    r#"{"type":"tool","name":"web_search","arguments":"{\"query\":\"atom tui output test\"}"}"#,
    r#"{"type":"tool_result","text":"atom — a chat client backed by a central session server\nhttps://github.com/andrewmiller/atom\nA TUI client for a central session server with tool blocks, reasoning, and compaction.\n\nBubble Tea — terminal UI framework\nhttps://github.com/charmbracelet/bubbletea\nThe Go framework atom's TUI is built on."}"#,
    r#"{"type":"content","text":"The search confirms the pieces exist. Let me look at the renderer to see how blocks are laid out.\n"}"#,
    r#"{"type":"tool","name":"read_file","arguments":"{\"path\":\"/Users/andrewmiller/repos/atom/tui.go\",\"offset\":3829,\"limit\":40}"}"#,
    r#"{"type":"tool_result","text":"hash: 9f2c1a8d4b7e3f60\n// renderBlock renders one conversation block, wrapped to width. While\n// reasoning display is off (see /thinking), a reasoning block collapses\n// to a summary line instead of its full text.\nfunc (m tuiModel) renderBlock(b *block, width int) string {\n\tswitch b.kind {\n\tcase \"user\":\n\t\treturn styleUser.Render(\"you: \") + wrapLinked(b.text, width, \"\", \"\") + \"\\n\\n\"\n\tcase \"assistant\":\n\t\treturn styleAssistant.Render(wrapLinked(b.text, width, colorForeground, \"\")) + \"\\n\\n\"\n\tcase \"reasoning\":\n\t\tif m.showReasoning {\n\t\t\treturn styleReasoning.Render(wrapLinked(b.text, width, colorMuted, \"\")) + \"\\n\\n\"\n\t\t}\n\t\treturn styleReasoning.Render(m.reasoningLabel(*b)) + \"\\n\\n\"\n\tcase \"compaction\":\n\t\t// Active compaction uses the reserved status row above the prompt,\n\t\t// in the same place as Working. Keep only completed compactions in\n\t\t// the conversation history.\n\t\tif b.active {\n\t\t\treturn \"\"\n\t\t}\n\t\ts := styleReasoning.Render(m.compactionLabel(*b)) + \"\\n\"\n\t\tif b.text != \"\" {\n\t\t\ts += styleAssistant.Render(wrapLinked(b.text, width, colorForeground, \"\")) + \"\\n\\n\"\n\t\t} else {\n\t\t\ts += \"\\n\"\n\t\t}\n\t\treturn s\n\tcase \"tool\":\n\t\tconst pad = 1\n\t\tinner := width - 2*pad"}"#,
    r#"{"type":"content","text":"renderBlock covers every block kind. Now I'll wire the scenes.\n"}"#,
    r#"{"type":"tool","name":"edit_file","arguments":"{\"path\":\"tui.go\",\"old_text\":\"const pad = 1\",\"new_text\":\"const pad = 2\"}"}"#,
    r#"{"type":"tool_result","text":"edited tui.go: replaced 14 bytes with 14 bytes"}"#,
    r#"{"type":"tool_diff","diff":"--- a/tui.go\n+++ b/tui.go\n@@ -3832,7 +3832,9 @@\n \tcase \"tool\":\n \t\tconst pad = 1\n \t\tinner := width - 2*pad\n+\t\t// Guard against a zero-width viewport: at least one cell keeps\n+\t\t// the box renderable.\n+\t\tif inner < 1 {\n+\t\t\tinner = 1\n+\t\t}\n \t\tif b.title != \"\" {"}"#,
    r#"{"type":"content","text":"The diff renders with added lines in green and the hunk header dim. Next, the build.\n"}"#,
    r#"{"type":"tool","name":"bash","arguments":"{\"command\":\"go test ./...\"}"}"#,
    r#"{"type":"tool_result","text":"ok  \tatom\t0.214s\nok  \tatom/cmd\t0.098s\nok  \tatom/cmd/install\t0.051s\nok  \tatom/internal\t0.177s"}"#,
    r#"{"type":"content","text":"All green. Let me check where the flag is referenced before wiring it up.\n"}"#,
    r#"{"type":"tool","name":"grep","arguments":"{\"pattern\":\"output-test\",\"path\":\"tui.go\"}"}"#,
    r#"{"type":"tool_result","text":"tui.go:5035: outputTest := flag.Bool(\"output-test\", false, ...)\ntui.go:5041: if *outputTest {\ntui.go:5046: runOutputTest()"}"#,
    r#"{"type":"content","text":"The flag routes to runOutputTest. Now let me collect the project files to sanity-check the layout.\n"}"#,
    r#"{"type":"tool","name":"glob","arguments":"{\"pattern\":\"**/*.go\"}"}"#,
    r#"{"type":"tool_result","text":"main.go\ntui.go\nserver.go\nsession.go\nmodels.go\nmodelsdev.go\ndispatch.go\noutputtest.go"}"#,
    r#"{"type":"content","text":"Everything is in place. Let me verify the render pipeline handles long outputs with a vector search.\n"}"#,
    r#"{"type":"tool","name":"vector_search","arguments":"{\"query\":\"render blocks in the tui\"}"}"#,
    r#"{"type":"tool_result","text":"tui.go:3829 renderBlock renders one conversation block, wrapped to width.\ntui.go:3898 renderBlocks renders the conversation blocks into the viewport text.\ntui.go:3901 refreshViewport updates the viewport from cached per-block lines."}"#,
    r#"{"type":"content","text":"Confirmed. I'll dispatch a subagent to double-check the scene ordering while I finalize.\n"}"#,
    r#"{"type":"tool","name":"dispatch","arguments":"{\"model\":\"deepseek-v4-flash:cloud\",\"prompt\":\"Review the output-test scene ordering in tui.go\"}"}"#,
    r#"{"type":"tool_result","text":"Subagent started (model deepseek-v4-flash:cloud). It is reviewing the output-test scene ordering and will report back with any issues."}"#,
    r#"{"type":"content","text":"While that runs, load the anti-ai skill to check the skill block renders with its description.\n"}"#,
    r#"{"type":"tool","name":"skill","arguments":"{\"name\":\"anti-ai\"}"}"#,
    r#"{"type":"tool_result","text":"Loaded skill anti-ai: rewrite AI-generated text so it passes as human — to human readers AND to statistical AI detectors (Pangram etc.), using empirically validated recipes (discourse fracture, interleave protocol). Only use this skill when the user explicitly invokes it by name."}"#,
    r#"{"type":"content","text":"The skill block renders its full instruction pack. Finally, write the flag documentation into the README.\n"}"#,
    r##"{"type":"tool","name":"write_file","arguments":"{\"path\":\"docs/output-test.md\",\"content\":\"# Output test\\n\\nRun atom --output-test to preview every block type and live state.\\n\"}"}"##,
    r#"{"type":"tool_result","text":"wrote docs/output-test.md (74 bytes)"}"#,
    r#"{"type":"content","text":"Done. --output-test is in place: the flag skips the server entirely, replays the scripted transcript through the real stream handlers, cycles Working, Thinking, and Compacting with live spinners, then settles on the full history. Run go build -o atom . and try ./atom --output-test to see every block type — tool calls with and without diffs, long collapsed results, reasoning, compaction, and errors — without a server or an API key."}"#,
    r#"{"type":"error","message":"provider returned 429: rate limit exceeded. The request will retry automatically."}"#,
    r#"{"type":"usage","prompt":"12345","completion":"678","total":"13023","cache_read":"9000","cache_write":"2100","prompt_all":"120000"}"#,
    r#"{"type":"done"}"#,
    r#"{"type":"compaction"}"#,
    r#"{"type":"compaction_end","text":"The user asked for an --output-test flag that replays a canned session covering every block type and live state. The demo replays a scripted transcript through the real stream handlers, cycles Working, Thinking, and Compacting, then settles on the full static view. All tool types are covered: web_search, read_file, write_file, edit_file with a diff, bash, grep, glob, vector_search, dispatch, and skill."}"#,
];

/// outputTestSceneDuration is how long each live-state scene stays up.
pub const OUTPUT_TEST_SCENE_DURATION: Duration = Duration::from_secs(3);

/// Live-state scenes for --output-test, in demo order.
pub const OUTPUT_TEST_SCENE_WORKING: i32 = 0;
pub const OUTPUT_TEST_SCENE_THINKING: i32 = 1;
pub const OUTPUT_TEST_SCENE_COMPACTING: i32 = 2;

fn parse_line(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or(serde_json::Value::Null)
}

/// replayOutputTest rebuilds blocks from the scripted transcript.
/// stop_at_reasoning_end keeps the reasoning block active for the
/// thinking scene; extra appends additional events (the lone compaction).
pub fn replay_output_test(app: &mut App, stop_at_reasoning_end: bool, extra: &[&str]) {
    app.blocks.clear();
    app.blocks.push(Block {
        kind: BlockKind::User,
        text: OUTPUT_TEST_PROMPT.to_string(),
        ..Default::default()
    });
    for line in OUTPUT_TEST_TRANSCRIPT {
        let v = parse_line(line);
        let ev = parse_stream_event(&v);
        if stop_at_reasoning_end && ev.event_type == "reasoning_end" {
            break;
        }
        app.handle_stream_event(&ev);
    }
    for line in extra {
        let ev = parse_stream_event(&parse_line(line));
        app.handle_stream_event(&ev);
    }
    // Backdate the completed compaction block for a realistic label;
    // leave a still-active compacting scene alone.
    for b in app.blocks.iter_mut().rev() {
        if b.kind == BlockKind::Compaction && !b.active {
            b.dur = Some(OUTPUT_TEST_SCENE_DURATION);
            break;
        }
    }
    app.refresh_viewport();
}

/// advanceOutputTestScene walks Thinking then Compacting, then returns
/// to the full transcript. Never clears the conversation or turns
/// streaming off.
pub fn advance_output_test_scene(app: &mut App) -> Vec<crate::events::Effect> {
    let was_streaming = app.streaming;
    let mut next_scene = true;
    match app.test_scene {
        OUTPUT_TEST_SCENE_WORKING | -1 => {
            app.test_scene = OUTPUT_TEST_SCENE_THINKING;
            replay_output_test(app, true, &[]);
        }
        OUTPUT_TEST_SCENE_THINKING => {
            app.test_scene = OUTPUT_TEST_SCENE_COMPACTING;
            replay_output_test(app, false, &[r#"{"type":"compaction"}"#]);
        }
        _ => {
            app.test_scene = OUTPUT_TEST_SCENE_WORKING;
            replay_output_test(app, false, &[]);
            next_scene = false;
        }
    }
    app.streaming = true;
    app.refresh_viewport();
    // The runtime re-arms the scene timer; nothing else to schedule.
    let _ = (next_scene, was_streaming);
    Vec::new()
}

/// Builds the output-test model before entering the UI loop.
pub fn output_test_app(hot_state_path: Option<std::path::PathBuf>) -> App {
    let mut session = crate::app::empty_session_info();
    session.title = "Output test".to_string();
    let mut m = App::new(RunOptions {
        providers: Vec::new(),
        sel_provider: atom_core::providers::providers::Provider {
            name: "output-test".into(),
            ..Default::default()
        },
        sel_model: "output-test".into(),
        session,
        hot_state_path,
    });
    m.test_mode = true;
    m.test_scene = OUTPUT_TEST_SCENE_WORKING;
    m.streaming = true;
    replay_output_test(&mut m, false, &[]);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_parses_and_replays_to_expected_blocks() {
        let mut app = App::new_test(100, 30);
        replay_output_test(&mut app, false, &[]);
        let kinds: Vec<BlockKind> = app.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds[0], BlockKind::User);
        assert_eq!(kinds[1], BlockKind::Reasoning);
        // Reasoning finalized with provider duration.
        assert!(!app.blocks[1].active);
        assert_eq!(app.blocks[1].dur, Some(Duration::from_millis(3200)));
        // Tool results attached to their calls (no orphan tool-only
        // blocks except where Go's flow creates them).
        let tools: Vec<&Block> = app
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Tool)
            .collect();
        assert!(tools.len() >= 9, "expected all tools, got {}", tools.len());
        assert!(
            tools.iter().all(|t| !t.result.is_empty()),
            "every call got a result"
        );
        // edit_file block carries the diff.
        let edit = tools
            .iter()
            .find(|t| t.tool_name == "edit_file")
            .expect("edit_file block");
        assert!(edit.diff.contains("--- a/tui.go"));
        // Error block present after final content.
        assert!(kinds.contains(&BlockKind::Error));
        // Usage stored on the session.
        let u = app.session.usage.expect("usage");
        assert_eq!(u.total_tokens, 13023);
        assert_eq!(u.prompt_tokens_all, 120000);
        // Compaction completed with backdated duration.
        let compaction = app
            .blocks
            .iter()
            .rev()
            .find(|b| b.kind == BlockKind::Compaction)
            .expect("compaction");
        assert!(!compaction.active);
        assert_eq!(compaction.dur, Some(OUTPUT_TEST_SCENE_DURATION));
        assert!(compaction.text.contains("--output-test"));
    }

    #[test]
    fn thinking_scene_stops_at_reasoning_end() {
        let mut app = App::new_test(100, 30);
        replay_output_test(&mut app, true, &[]);
        let reasoning = app
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Reasoning)
            .next_back()
            .expect("reasoning block");
        assert!(
            reasoning.active,
            "reasoning still streaming in thinking scene"
        );
    }

    #[test]
    fn compacting_scene_appends_active_phase() {
        let mut app = App::new_test(100, 30);
        replay_output_test(&mut app, false, &[r#"{"type":"compaction"}"#]);
        let active = app
            .blocks
            .iter()
            .any(|b| b.kind == BlockKind::Compaction && b.active);
        assert!(active);
        assert!(app
            .content_lines
            .iter()
            .any(|line| crate::ansi::line_plain(line).contains("Compacting")));
    }

    #[test]
    fn scene_walk_returns_to_working_full_view() {
        let mut app = crate::outputtest::output_test_app(None);
        assert_eq!(app.test_scene, OUTPUT_TEST_SCENE_WORKING);
        app.test_scene = OUTPUT_TEST_SCENE_COMPACTING;
        advance_output_test_scene(&mut app);
        assert_eq!(app.test_scene, OUTPUT_TEST_SCENE_WORKING);
        assert!(app.streaming, "demo stays live until quit");
        assert!(
            !app.blocks
                .iter()
                .any(|b| b.kind == BlockKind::Compaction && b.active),
            "final view has no active compaction"
        );
    }
}
