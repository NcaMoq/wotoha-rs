use std::{
    collections::HashMap,
    io::{self, Read, Write},
};

use crate::{ChallengeInput, ChallengeOutput, SolverSession, prepare_player};
use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 12 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SESSIONS: usize = 4;
const MAX_JOBS: usize = 64;
const MAX_VALUE_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
struct WorkerRequest {
    protocol_version: u32,
    #[serde(default)]
    request_id: Option<u64>,
    player_key: String,
    player_source: Option<String>,
    inputs: Vec<ChallengeInput>,
    #[serde(default)]
    per_input_results: bool,
}

#[derive(Serialize)]
struct WorkerResponse {
    protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
    outputs: Option<Vec<ChallengeOutput>>,
    results: Option<Vec<WorkerChallengeResult>>,
    error: Option<String>,
}

#[derive(Serialize)]
struct WorkerChallengeResult {
    output: Option<ChallengeOutput>,
    error: Option<String>,
}

type WorkerResultPayload = (
    Option<Vec<ChallengeOutput>>,
    Option<Vec<WorkerChallengeResult>>,
);

pub fn run_worker() {
    if apply_process_limits().is_err() {
        std::process::exit(70);
    }
    let worker = std::thread::Builder::new()
        .name("youtube-js-worker".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(run_worker_loop);
    match worker.and_then(|worker| {
        worker
            .join()
            .map_err(|_| io::Error::other("worker thread panicked"))
    }) {
        Ok(()) => {}
        Err(_) => std::process::exit(70),
    }
}

fn run_worker_loop() {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut sessions = HashMap::<String, SolverSession>::new();
    while let Ok(frame) = read_frame(&mut input) {
        let response = handle_request(&mut sessions, &frame);
        let encoded = serde_json::to_vec(&response).unwrap_or_else(|_| {
            br#"{"protocol_version":1,"outputs":null,"error":"response encoding failed"}"#.to_vec()
        });
        if encoded.len() > MAX_RESPONSE_BYTES || write_frame(&mut output, &encoded).is_err() {
            break;
        }
    }
}

fn handle_request(sessions: &mut HashMap<String, SolverSession>, frame: &[u8]) -> WorkerResponse {
    let request = serde_json::from_slice::<WorkerRequest>(frame)
        .map_err(|_| "invalid request JSON".to_owned());
    let request_id = request.as_ref().ok().and_then(|request| request.request_id);
    let result: Result<WorkerResultPayload, String> = (|| {
        let request = request?;
        validate_request(&request)?;
        if !sessions.contains_key(&request.player_key) {
            let source = request
                .player_source
                .as_deref()
                .ok_or_else(|| "player source is required for a new session".to_owned())?;
            if sessions.len() >= MAX_SESSIONS {
                sessions.clear();
            }
            let prepared = prepare_player(source).map_err(|error| error.to_string())?;
            let session = SolverSession::new(&prepared).map_err(|error| error.to_string())?;
            sessions.insert(request.player_key.clone(), session);
        }
        let session = sessions
            .get_mut(&request.player_key)
            .ok_or_else(|| "player session was not created".to_owned())?;
        if request.per_input_results {
            let results = session
                .solve_batch_isolated(&request.inputs)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|result| match result {
                    Ok(output) => WorkerChallengeResult {
                        output: Some(output),
                        error: None,
                    },
                    Err(_) => WorkerChallengeResult {
                        output: None,
                        error: Some("no_unique_solution".to_owned()),
                    },
                })
                .collect::<Vec<_>>();
            validate_results(&results, request.inputs.len())?;
            Ok((None, Some(results)))
        } else {
            let outputs = session
                .solve_batch(&request.inputs)
                .map_err(|error| error.to_string())?;
            validate_outputs(&outputs, request.inputs.len())?;
            Ok((Some(outputs), None))
        }
    })();
    match result {
        Ok((outputs, results)) => WorkerResponse {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            outputs,
            results,
            error: None,
        },
        Err(error) => WorkerResponse {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            outputs: None,
            results: None,
            error: Some(sanitize_error(&error)),
        },
    }
}

fn validate_request(request: &WorkerRequest) -> Result<(), String> {
    if request.protocol_version != PROTOCOL_VERSION {
        return Err("unsupported protocol version".to_owned());
    }
    if request.player_key.is_empty() || request.player_key.len() > 4096 {
        return Err("invalid player key".to_owned());
    }
    if request.inputs.is_empty() || request.inputs.len() > MAX_JOBS {
        return Err("invalid challenge job count".to_owned());
    }
    if request.inputs.iter().any(|input| {
        input
            .signature
            .as_ref()
            .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
            || input
                .n
                .as_ref()
                .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
    }) {
        return Err("challenge input exceeded its size limit".to_owned());
    }
    Ok(())
}

fn validate_outputs(outputs: &[ChallengeOutput], expected_count: usize) -> Result<(), String> {
    if outputs.len() != expected_count
        || outputs.len() > MAX_JOBS
        || outputs.iter().any(|output| {
            output
                .signature
                .as_ref()
                .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
                || output
                    .n
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
        })
    {
        return Err("challenge output exceeded its size limit".to_owned());
    }
    Ok(())
}

fn validate_results(
    results: &[WorkerChallengeResult],
    expected_count: usize,
) -> Result<(), String> {
    if results.len() != expected_count
        || results.len() > MAX_JOBS
        || results.iter().any(|result| match &result.output {
            Some(output) => {
                result.error.is_some() || validate_outputs(std::slice::from_ref(output), 1).is_err()
            }
            None => result
                .error
                .as_ref()
                .is_none_or(|error| error.is_empty() || error.len() > 512),
        })
    {
        return Err("challenge result exceeded its size limit".to_owned());
    }
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request frame length",
        ));
    }
    let mut frame = vec![0_u8; length];
    reader.read_exact(&mut frame)?;
    Ok(frame)
}

fn write_frame(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    writer.write_all(&(frame.len() as u32).to_be_bytes())?;
    writer.write_all(frame)?;
    writer.flush()
}

fn sanitize_error(error: &str) -> String {
    error
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}

#[cfg(target_os = "linux")]
fn apply_process_limits() -> io::Result<()> {
    const WORKER_ADDRESS_SPACE_LIMIT: libc::rlim_t = 1024 * 1024 * 1024;
    let limit = libc::rlimit {
        rlim_cur: WORKER_ADDRESS_SPACE_LIMIT,
        rlim_max: WORKER_ADDRESS_SPACE_LIMIT,
    };
    // SAFETY: these calls modify only this helper process before Player code is parsed.
    unsafe {
        if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::getppid() == 1 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "worker parent exited during startup",
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_process_limits() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER_FIXTURE: &str = r#"
var _player = {};
(function(g) {
  function Param() { this.values = new Map(); }
  Param.prototype.set = function(key, value) { this.values.set(key, value); };
  Param.prototype.get = function(key) { return this.values.get(key); };
  Param.prototype.clone = function() { return this; };
  Param.prototype.transform = function() {
    const n = this.values.get("n");
    if (n) this.values.set("n", n.slice(1) + n[0]);
  };
  function solve(a, b, c) {
    const value = new Param();
    value.set("alr", "yes");
    return value;
  }
})(_player);
"#;

    #[test]
    fn reuses_session_without_resending_player_source() {
        let mut sessions = HashMap::new();
        for (source, input, expected) in [
            (Some(PLAYER_FIXTURE), "1234", "2341"),
            (None, "abcd", "bcda"),
        ] {
            let frame = serde_json::to_vec(&serde_json::json!({
                "protocol_version": 1,
                "player_key": "fixture",
                "player_source": source,
                "inputs": [{"signature": null, "n": input}]
            }))
            .unwrap();
            let response = handle_request(&mut sessions, &frame);
            assert!(response.error.is_none());
            assert_eq!(response.outputs.unwrap()[0].n.as_deref(), Some(expected));
        }
    }

    #[test]
    fn reports_per_input_failures_without_rejecting_the_batch() {
        let source = PLAYER_FIXTURE.replace(
            "const value = new Param();",
            r#"
    if (c && decodeURIComponent(c) === "reject") throw new Error("rejected");
    const value = new Param();"#,
        );
        let frame = serde_json::to_vec(&serde_json::json!({
            "protocol_version": 1,
            "player_key": "partial-fixture",
            "player_source": source,
            "per_input_results": true,
            "inputs": [
                {"signature": "reject", "n": null},
                {"signature": null, "n": "1234"}
            ]
        }))
        .unwrap();
        let response = handle_request(&mut HashMap::new(), &frame);
        assert!(response.error.is_none());
        assert!(response.outputs.is_none());
        let results = response.results.unwrap();
        assert_eq!(results[0].error.as_deref(), Some("no_unique_solution"));
        assert!(results[0].output.is_none());
        assert_eq!(
            results[1].output.as_ref().unwrap().n.as_deref(),
            Some("2341")
        );
        assert!(results[1].error.is_none());
    }

    #[test]
    fn framed_protocol_round_trips() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, br#"{"protocol_version":1}"#).unwrap();
        assert_eq!(
            read_frame(&mut bytes.as_slice()).unwrap(),
            br#"{"protocol_version":1}"#
        );
    }
}
