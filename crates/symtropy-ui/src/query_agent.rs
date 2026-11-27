//! Local query planner for Tag Manager's AI query.
//!
//! This is deliberately proposal-only. A local agent process receives the user's
//! prompt plus the library's known tag catalog and returns a JSON query plan.
//! The plan is parsed into the normal `Query` AST, validated against the same
//! catalog, and must still be applied with an explicit Rebuild/Add click.
//!
//! Nothing here writes tags, sidecars, or the review store. The default backend
//! is a locally installed Ollama model, contacted through the local CLI; no
//! remote fetch or API key is used. `SYMTROPY_QUERY_AGENT` can override the
//! command, and `SYMTROPY_QUERY_AGENT_DISABLED=1` turns planning off.

use crate::gallery::{hierarchy::tag_key, Query};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";

#[derive(Clone, Debug)]
pub struct PlanRequest {
    pub id: u64,
    pub prompt: String,
    pub catalog: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PlanResult {
    pub id: u64,
    pub query: Result<Query, String>,
}

#[derive(Deserialize)]
struct AgentPlan {
    query: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Nonblocking handle to one local query-planning worker.
pub struct QueryPlanner {
    requests: Sender<PlanRequest>,
    results: Receiver<PlanResult>,
    next_id: u64,
    busy: bool,
}

impl Default for QueryPlanner {
    fn default() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PlanRequest>();
        let (result_tx, result_rx) = mpsc::channel::<PlanResult>();
        thread::spawn(move || {
            for request in request_rx {
                let query = plan(&request);
                let _ = result_tx.send(PlanResult { id: request.id, query });
            }
        });
        Self { requests: request_tx, results: result_rx, next_id: 0, busy: false }
    }
}

impl QueryPlanner {
    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn request(&mut self, prompt: String, catalog: Vec<String>) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.busy = true;
        if self.requests.send(PlanRequest { id: self.next_id, prompt, catalog }).is_err() {
            self.busy = false;
        }
        self.next_id
    }

    pub fn poll(&mut self) -> Option<PlanResult> {
        match self.results.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                None
            }
        }
    }
}

fn agent_command() -> Result<(String, Vec<String>), String> {
    if std::env::var_os("SYMTROPY_QUERY_AGENT_DISABLED").is_some_and(|v| v != "0" && v != "false") {
        return Err("local query agent is disabled".to_string());
    }
    if let Some(command) = std::env::var("SYMTROPY_QUERY_AGENT").ok().filter(|s| !s.trim().is_empty()) {
        let mut parts = command.split_whitespace().map(str::to_string);
        let program = parts.next().ok_or_else(|| "empty query agent command".to_string())?;
        return Ok((program, parts.collect()));
    }
    Ok((
        "ollama".to_string(),
        vec![
            "run".to_string(),
            std::env::var("SYMTROPY_QUERY_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            "--format".to_string(),
            "json".to_string(),
            "--nowordwrap".to_string(),
        ],
    ))
}

fn planner_prompt(prompt: &str, catalog: &[String]) -> String {
    format!(
        "Translate an image search phrase into Symtropy query DSL.\n\
         Return JSON only: {{\"query\":\"<dsl>\",\"tags\":[\"<allowed tag>\"]}}.\n\
         Use + for AND, | for OR, ! for NOT. Do not negate unless the user says not/without.\n\
         Use only these known tags: {}\n\
         If the phrase names something absent from the known tags, use your own world knowledge \
         to choose the closest known tags. Do not invent tags. Do not add commentary.\n\
         Phrase: {prompt}\n",
        catalog.join(", ")
    )
}

fn ensure_default_model_present(model: &str) -> Result<(), String> {
    let status = Command::new("ollama")
        .arg("show")
        .arg(model)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("cannot check local model '{model}': {error}"))?;
    if !status.success() {
        return Err(format!("local model '{model}' is not installed"));
    }
    Ok(())
}

fn run_agent(prompt_text: &str) -> Result<String, String> {
    let (program, args) = agent_command()?;
    if program == "ollama" && args.first().is_some_and(|arg| arg == "run") {
        if let Some(model) = args.get(1) {
            ensure_default_model_present(model)?;
        }
    }
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start local query agent '{program}': {error}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(prompt_text.as_bytes())
            .map_err(|error| format!("could not send prompt to local query agent: {error}"))?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| format!("local query agent failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output
            .status
            .code()
            .map_or_else(|| "abnormally".to_string(), |c| c.to_string());
        return Err(if stderr.trim().is_empty() {
            format!("local query agent exited {status}")
        } else {
            format!("local query agent exited {status}: {}", stderr.trim())
        });
    }
    String::from_utf8(output.stdout).map_err(|error| format!("query agent output was not UTF-8: {error}"))
}

fn plan(request: &PlanRequest) -> Result<Query, String> {
    let raw = run_agent(&planner_prompt(&request.prompt, &request.catalog))?;
    let start = raw.find('{').ok_or_else(|| format!("query agent did not return JSON: {}", raw.trim()))?;
    let end = raw.rfind('}').ok_or_else(|| "query agent returned truncated JSON".to_string())?;
    let plan: AgentPlan = serde_json::from_str(&raw[start..=end])
        .map_err(|error| format!("query agent returned invalid JSON: {error}"))?;
    let parsed = Query::parse(&plan.query).map_err(|error| format!("invalid planned query '{}': {error}", plan.query))?;

    let allowed: BTreeSet<String> = request
        .catalog
        .iter()
        .chain(plan.tags.iter())
        .map(|tag| tag_key(tag))
        .filter(|tag| request.catalog.iter().any(|known| tag_key(known) == *tag))
        .collect();
    let validated = parsed.map_tags(&|tag| {
        let key = tag_key(tag);
        allowed.contains(&key).then_some(key)
    });
    if matches!(validated, Query::AlwaysTrue) {
        return Err(format!("query agent used no known tags: {}", plan.query));
    }
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_prompt_asks_for_known_tags_only() {
        let prompt = planner_prompt("sprigatito", &["pokemon".into(), "feline".into()]);
        assert!(prompt.contains("pokemon, feline"));
        assert!(prompt.contains("Phrase: sprigatito"));
        assert!(prompt.contains("Do not invent tags"));
    }

    /// Opt-in smoke test for the locally installed model. Ignored by default
    /// because model files are never bundled or fetched by the test suite.
    #[test]
    #[ignore = "requires a local query agent/model"]
    fn local_agent_maps_unknown_entities_to_known_tags() {
        let request = PlanRequest {
            id: 1,
            prompt: "sprigatito".to_string(),
            catalog: vec![
                "pokemon".to_string(),
                "feline".to_string(),
                "house-cat".to_string(),
                "cartoony".to_string(),
            ],
        };
        let query = plan(&request).expect("local model should return a valid plan");
        let text = query.to_string();
        assert!(text.contains("pokemon") || text.contains("feline"), "unexpected plan: {text}");
    }

    #[test]
    fn agent_plans_are_validated_against_the_catalog() {
        let plan: AgentPlan =
            serde_json::from_str(r#"{"query":"pokemon + sprite","tags":["pokemon","sprite"]}"#).unwrap();
        let parsed = Query::parse(&plan.query).unwrap();
        let allowed: BTreeSet<String> = ["pokemon".to_string()].into_iter().collect();
        let validated = parsed.map_tags(&|tag| {
            let key = tag_key(tag);
            allowed.contains(&key).then_some(key)
        });
        assert_eq!(
            validated,
            Query::And(vec![Query::Tag("pokemon".to_string())])
        );
    }
}
