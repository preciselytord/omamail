//! Bounded stdio MCP transport. The launch binds ownership, not tool arguments.
use super::{jobs, proposals, storage::Store};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub fn run(id: &str) -> Result<(), &'static str> {
    if id == "workspace" {
        let cwd = std::env::current_dir().map_err(|_| "agent_unsafe_storage")?;
        let job = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("agent_invalid_id")?;
        super::storage::check_id(job)?;
        {
            let store = Store::open()?;
            if cwd != store.path().join(job) {
                return Err("agent_unsafe_storage");
            }
        }
        return run(job);
    }
    super::storage::check_id(id)?;
    serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        |tool, arguments| match tool {
            Tool::ReadCalendar => {
                // The same turn ownership as a proposal: a finished, cancelled
                // or look job's server reads nothing.
                Store::open()
                    .and_then(|store| active(&store, id))
                    .map_err(|_| "agent_proposal_inactive")?;
                super::calendar::read(arguments)
            }
            Tool::ProposeDraft => {
                let content = proposals::validate(arguments)?;
                record(
                    id,
                    "proposals.json",
                    proposals::MAX_PROPOSALS,
                    |store, list, after_turn| {
                        let job = jobs::read_job(store, id)?;
                        let proposal_id = format!("{id}-{}", list.len());
                        list.push(
                            json!({"id":proposal_id,"jobId":id,"accountId":job["accountId"],
                        "afterTurn":after_turn,
                        "draftKey":job["draftKey"],"messageId":job["messageId"],
                        "subject":content["subject"],"body":content["body"]}),
                        );
                        Ok(proposal_id)
                    },
                )
            }
            Tool::ProposeEvent => {
                let event = super::calendar::validate_proposal(arguments)?;
                record(
                    id,
                    "event-proposals.json",
                    super::calendar::MAX_PROPOSALS,
                    |store, list, after_turn| {
                        let job = jobs::read_job(store, id)?;
                        let proposal_id = format!("{id}-e{}", list.len());
                        let mut item = event.clone();
                        for (key, value) in [
                            ("id", json!(proposal_id)),
                            ("jobId", json!(id)),
                            ("accountId", job["accountId"].clone()),
                            ("messageId", job["messageId"].clone()),
                            ("afterTurn", json!(after_turn)),
                        ] {
                            item[key] = value;
                        }
                        list.push(item);
                        Ok(proposal_id)
                    },
                )
            }
        },
    )
}

/// Append one proposal under the job's lock-free ownership checks shared by
/// both kinds: the turn is running, not a look, and not cancelled.
fn record(
    id: &str,
    file: &str,
    limit: usize,
    push: impl FnOnce(&Store, &mut Vec<Value>, usize) -> Result<String, &'static str>,
) -> Result<String, &'static str> {
    let store = Store::open()?;
    active(&store, id)?;
    let mut saved = store
        .read_json(id, file, proposals::MAX_BYTES)?
        .unwrap_or_else(|| json!([]));
    let list = saved.as_array_mut().ok_or("agent_invalid_proposal")?;
    if list.len() >= limit {
        return Err("agent_proposal_limit");
    }
    let display = jobs::saved_display(&store, id)?;
    let after_turn = display["transcript"].as_array().map_or(0, Vec::len);
    let proposal_id = push(&store, list, after_turn)?;
    if serde_json::to_vec(&saved)
        .map_err(|_| "agent_invalid_proposal")?
        .len()
        > proposals::MAX_BYTES
    {
        return Err("agent_proposal_limit");
    }
    store.write_json(id, file, &saved)?;
    Ok(format!(
        "{} {proposal_id} recorded for the owner's review. {}",
        if file == "proposals.json" {
            "Proposal"
        } else {
            "Event suggestion"
        },
        if file == "proposals.json" {
            "No mail was edited or sent."
        } else {
            "No calendar was changed."
        }
    ))
}

/// Whether the job may still use its tools: running, not a look, and not
/// cancelled. Cancellation writes cancel.json before signalling the worker.
fn active(store: &Store, id: &str) -> Result<(), &'static str> {
    let job = jobs::refresh(store, id)?;
    if job["state"] != "running"
        || job["kind"] == "events"
        || store.read_json(id, "cancel.json", 64)?.is_some()
    {
        return Err("agent_proposal_inactive");
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    ProposeDraft,
    ReadCalendar,
    ProposeEvent,
}

impl Tool {
    fn named(name: &str) -> Option<Self> {
        match name {
            "propose_draft" => Some(Self::ProposeDraft),
            "read_calendar" => Some(Self::ReadCalendar),
            "propose_event" => Some(Self::ProposeEvent),
            _ => None,
        }
    }
    fn refusal(self, reason: &str) -> String {
        match self {
            Self::ProposeDraft => format!(
                "Proposal refused ({reason}). Check subject/body and whether this turn is still active."
            ),
            Self::ProposeEvent => format!(
                "Event suggestion refused ({reason}). Check title, start and end, and whether this turn is still active."
            ),
            Self::ReadCalendar => match reason {
                "agent_calendar_invalid" | "agent_calendar_range" => format!(
                    "Calendar read refused ({reason}). Ask for one window of at most 31 days as from/to YYYY-MM-DD."
                ),
                "agent_proposal_inactive" => {
                    format!("Calendar read refused ({reason}): this turn is no longer active.")
                }
                _ => format!(
                    "The calendar could not be read ({reason}). Tell the owner their calendar is unavailable; do not treat any day as free."
                ),
            },
        }
    }
}

fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    mut handle: impl FnMut(Tool, &Value) -> Result<String, &'static str>,
) -> Result<(), &'static str> {
    let mut initialized = false;
    let mut processed = 0;
    // Same request IDs are replay-safe for this connection; bound memory too.
    let mut replies: Vec<(Value, Value)> = Vec::new();
    loop {
        let mut line = Vec::new();
        let size = std::io::Read::take(&mut input, 128 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| "agent_mcp_io")?;
        if size == 0 {
            return Ok(());
        }
        if size > 128 * 1024 || line.last() != Some(&b'\n') {
            return Err("agent_mcp_limit");
        }
        processed += 1;
        if processed > 256 {
            return Err("agent_mcp_limit");
        }
        let request: Value = serde_json::from_slice(&line).map_err(|_| "agent_mcp_invalid")?;
        if request["jsonrpc"] != "2.0" {
            return Err("agent_mcp_invalid");
        }
        let Some(id) = request.get("id") else {
            continue;
        };
        if !(id.is_number() || id.as_str().is_some_and(|s| s.len() <= 128)) {
            return Err("agent_mcp_invalid");
        }
        let response = if let Some((_, response)) = replies.iter().find(|(known, _)| known == id) {
            response.clone()
        } else {
            let result = match request["method"].as_str().unwrap_or("") {
                "initialize" => {
                    initialized = true;
                    Ok(
                        json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},
                        "serverInfo":{"name":"omamail","version":env!("CARGO_PKG_VERSION")}}),
                    )
                }
                "ping" => Ok(json!({})),
                "tools/list" if initialized => Ok(
                    json!({"tools":[proposals::tool(),super::calendar::read_tool(),super::calendar::propose_tool()]}),
                ),
                "resources/list" if initialized => Ok(json!({"resources":[]})),
                "resources/templates/list" if initialized => Ok(json!({"resourceTemplates":[]})),
                "tools/call"
                    if initialized
                        && request["params"]["name"]
                            .as_str()
                            .and_then(Tool::named)
                            .is_some() =>
                {
                    let tool = Tool::named(request["params"]["name"].as_str().unwrap()).unwrap();
                    match handle(tool, &request["params"]["arguments"]) {
                        Ok(text) => Ok(json!({"content":[{"type":"text","text":text}]})),
                        Err(reason) => Ok(
                            json!({"isError":true,"content":[{"type":"text","text":tool.refusal(reason)}]}),
                        ),
                    }
                }
                _ => Err(json!({"code":-32601,"message":"Unsupported MCP method or tool"})),
            };
            let response = match result {
                Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
            };
            replies.push((id.clone(), response.clone()));
            response
        };
        serde_json::to_writer(&mut output, &response).map_err(|_| "agent_mcp_io")?;
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|_| "agent_mcp_io")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_and_replayed_calls_cannot_create_extra_proposals() {
        let request = |id, method, params| {
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string() + "\n"
        };
        let valid = request(
            3,
            "tools/call",
            json!({"name":"propose_draft","arguments":{"subject":"Hello","body":"Text"}}),
        );
        let wire = request(1, "initialize", json!({}))
            + &request(
                2,
                "tools/call",
                json!({"name":"propose_draft","arguments":{"subject":"Hello\nBcc: injected","body":"Text"}}),
            )
            + &valid
            + &valid
            + &request(4, "tools/call", json!({"name":"send_email","arguments":{}}));
        let mut effects = 0;
        let mut output = Vec::new();
        serve(wire.as_bytes(), &mut output, |tool, args| {
            assert_eq!(tool, Tool::ProposeDraft);
            proposals::validate(args)?;
            effects += 1;
            Ok("synthetic-1".into())
        })
        .unwrap();
        assert_eq!(effects, 1);
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(responses[2], responses[3]);
        assert!(responses[4].get("error").is_some());
    }

    #[test]
    fn calendar_tools_route_by_name_and_refusals_say_which_tool() {
        let request = |id, method, params| {
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string() + "\n"
        };
        let wire = request(1, "initialize", json!({}))
            + &request(2, "tools/list", json!({}))
            + &request(
                3,
                "tools/call",
                json!({"name":"read_calendar","arguments":{"from":"2030-03-01"}}),
            )
            + &request(
                4,
                "tools/call",
                json!({"name":"propose_event","arguments":{"title":"Dinner","start":"2030-03-05T19:00"}}),
            )
            + &request(
                5,
                "tools/call",
                json!({"name":"propose_event","arguments":{"title":"Dinner","start":"2030-03-05T19:00","calendar":"work"}}),
            )
            + &request(
                6,
                "tools/call",
                json!({"name":"write_event","arguments":{}}),
            );
        let mut calls = Vec::new();
        let mut output = Vec::new();
        serve(wire.as_bytes(), &mut output, |tool, args| {
            if tool == Tool::ProposeEvent {
                super::super::calendar::validate_proposal(args)?;
            }
            calls.push(tool);
            Ok(format!("{tool:?} ok"))
        })
        .unwrap();
        assert_eq!(calls, [Tool::ReadCalendar, Tool::ProposeEvent]);
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let names: Vec<_> = responses[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["propose_draft", "read_calendar", "propose_event"]);
        assert_eq!(
            responses[2]["result"]["content"][0]["text"],
            "ReadCalendar ok"
        );
        assert_eq!(responses[4]["result"]["isError"], true);
        assert!(
            responses[4]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Event suggestion refused")
        );
        assert!(responses[5].get("error").is_some());
    }

    #[test]
    fn oversized_input_is_refused_before_any_effect() {
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string() + "\n";
        let request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"propose_draft","arguments":{"subject":"Hello","body":"Text"}},"padding":""}).to_string();
        for size in [128 * 1024, 128 * 1024 + 1] {
            let mut value: Value = serde_json::from_str(&request).unwrap();
            value["padding"] = json!("x".repeat(size - request.len() - 1));
            let frame = value.to_string() + "\n";
            assert_eq!(frame.len(), size);
            let wire = initialize.clone() + &frame;
            let mut effects = 0;
            let result = serve(wire.as_bytes(), Vec::new(), |_, args| {
                proposals::validate(args)?;
                effects += 1;
                Ok("synthetic-1".into())
            });
            if size == 128 * 1024 {
                assert_eq!(result, Ok(()));
                assert_eq!(
                    effects, 1,
                    "the same valid proposal reaches the observer at the limit"
                );
            } else {
                assert_eq!(result, Err("agent_mcp_limit"));
                assert_eq!(effects, 0);
            }
        }
    }
}
