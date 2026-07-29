use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};

use serde_json::{Value, json};

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
fn binary_protocol_reuses_player_session() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wotoha-youtube-js-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    for (request_id, source, input, expected) in [
        (40, Some(PLAYER_FIXTURE), "1234", "2341"),
        (41, None, "abcd", "bcda"),
    ] {
        let request = serde_json::to_vec(&json!({
            "protocol_version": 1,
            "request_id": request_id,
            "player_key": "fixture",
            "player_source": source,
            "inputs": [{"signature": null, "n": input}]
        }))
        .unwrap();
        write_frame(&mut stdin, &request);
        let response: Value = serde_json::from_slice(&read_frame(&mut stdout)).unwrap();
        assert_eq!(response["request_id"], request_id);
        assert_eq!(response["error"], Value::Null);
        assert_eq!(response["outputs"][0]["n"], expected);
    }
    let request = serde_json::to_vec(&json!({
        "protocol_version": 1,
        "request_id": 42,
        "player_key": "fixture",
        "player_source": null,
        "per_input_results": true,
        "inputs": [{"signature": null, "n": "wxyz"}]
    }))
    .unwrap();
    write_frame(&mut stdin, &request);
    let response: Value = serde_json::from_slice(&read_frame(&mut stdout)).unwrap();
    assert_eq!(response["request_id"], 42);
    assert_eq!(response["error"], Value::Null);
    assert_eq!(response["outputs"], Value::Null);
    assert_eq!(response["results"][0]["output"]["n"], "xyzw");
    assert_eq!(response["results"][0]["error"], Value::Null);

    child.kill().unwrap();
    child.wait().unwrap();
}

fn write_frame(writer: &mut impl Write, frame: &[u8]) {
    writer
        .write_all(&(frame.len() as u32).to_be_bytes())
        .unwrap();
    writer.write_all(frame).unwrap();
    writer.flush().unwrap();
}

fn read_frame(reader: &mut impl Read) -> Vec<u8> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).unwrap();
    let mut frame = vec![0_u8; u32::from_be_bytes(header) as usize];
    reader.read_exact(&mut frame).unwrap();
    frame
}
