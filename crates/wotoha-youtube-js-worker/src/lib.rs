use std::collections::HashSet;

use boa_engine::{Context, Source};
use oxc::{
    allocator::Allocator,
    ast::ast::{
        Argument, AssignmentOperator, BindingPattern, CallExpression, Expression, Function,
        FunctionBody, Program, Statement,
    },
    ast_visit::{Visit, walk},
    parser::Parser,
    span::{GetSpan, SourceType, Span},
};
use serde::{Deserialize, Serialize};

mod process;

pub use process::run_worker;

// Candidate discovery follows the structural AST approach documented by yt-dlp/ejs rather than
// matching minified Player source with version-specific regular expressions:
// https://github.com/yt-dlp/ejs
const SETUP_SCRIPT: &str = r#"
if (typeof globalThis.XMLHttpRequest === "undefined") {
  globalThis.XMLHttpRequest = { prototype: {} };
}
if (typeof URL === "undefined") {
  globalThis.location = {
    hash: "", host: "www.youtube.com", hostname: "www.youtube.com",
    href: "https://www.youtube.com/watch?v=wotoha", origin: "https://www.youtube.com",
    password: "", pathname: "/watch", port: "", protocol: "https:",
    search: "?v=wotoha", username: ""
  };
} else {
  globalThis.location = new URL("https://www.youtube.com/watch?v=wotoha");
}
if (typeof globalThis.document === "undefined") globalThis.document = Object.create(null);
if (typeof globalThis.navigator === "undefined") globalThis.navigator = Object.create(null);
if (typeof globalThis.self === "undefined") globalThis.self = globalThis;
if (typeof globalThis.window === "undefined") globalThis.window = globalThis;
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChallengeInput {
    pub signature: Option<String>,
    pub n: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChallengeOutput {
    pub signature: Option<String>,
    pub n: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedPlayer {
    code: String,
    candidate_count: usize,
}

pub struct SolverSession {
    context: Context,
}

#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    #[error("player JavaScript exceeded the 8 MiB limit")]
    PlayerTooLarge,
    #[error("player JavaScript could not be parsed: {0}")]
    Parse(String),
    #[error("player JavaScript had an unsupported top-level structure")]
    Structure,
    #[error("player JavaScript exposed no challenge function candidates")]
    NoCandidates,
    #[error("player JavaScript exposed too many challenge function candidates")]
    TooManyCandidates,
    #[error("player JavaScript setup failed: {0}")]
    Setup(String),
    #[error("player JavaScript challenge execution failed: {0}")]
    Execute(String),
    #[error("player JavaScript challenge returned invalid JSON: {0}")]
    InvalidOutput(serde_json::Error),
    #[error("player JavaScript challenge returned no unique solution")]
    NoUniqueSolution,
}

pub fn prepare_player(source: &str) -> Result<PreparedPlayer, SolverError> {
    if source.len() > 8 * 1024 * 1024 {
        return Err(SolverError::PlayerTooLarge);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::default()).parse();
    if !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .iter()
            .take(3)
            .map(|error| format!("{error:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(SolverError::Parse(message));
    }
    let body = player_body(&parsed.program).ok_or(SolverError::Structure)?;
    let statements = body.statements.as_slice();
    let candidates = challenge_candidates(statements, source);
    if candidates.is_empty() {
        return Err(SolverError::NoCandidates);
    }
    if candidates.len() > 32 {
        return Err(SolverError::TooManyCandidates);
    }

    let mut filtered = source.as_bytes().to_vec();
    for (index, statement) in statements.iter().enumerate() {
        if (index == 0 && is_window_alias(statement, source)) || !keep_player_statement(statement) {
            let span = statement.span();
            let range = span.start as usize..span.end as usize;
            let Some(bytes) = filtered.get_mut(range) else {
                return Err(SolverError::Structure);
            };
            bytes.fill(b' ');
        }
    }
    let filtered = String::from_utf8(filtered).map_err(|_| SolverError::Structure)?;
    let insertion = body.span.end.checked_sub(1).ok_or(SolverError::Structure)? as usize;
    if !filtered.is_char_boundary(insertion) {
        return Err(SolverError::Structure);
    }
    let mut registration = String::from("\nglobalThis.__wotoha_candidates=[\n");
    for candidate in &candidates {
        registration.push_str(&candidate_wrapper(candidate));
        registration.push_str(",\n");
    }
    registration.push_str("];\n");
    let mut code = String::with_capacity(SETUP_SCRIPT.len() + filtered.len() + registration.len());
    code.push_str(SETUP_SCRIPT);
    code.push_str(&filtered[..insertion]);
    code.push_str(&registration);
    code.push_str(&filtered[insertion..]);

    Ok(PreparedPlayer {
        code,
        candidate_count: candidates.len(),
    })
}

#[cfg(test)]
pub fn solve(
    prepared: &PreparedPlayer,
    input: &ChallengeInput,
) -> Result<ChallengeOutput, SolverError> {
    solve_batch(prepared, std::slice::from_ref(input))?
        .into_iter()
        .next()
        .ok_or(SolverError::NoUniqueSolution)
}

#[cfg(test)]
pub fn solve_batch(
    prepared: &PreparedPlayer,
    inputs: &[ChallengeInput],
) -> Result<Vec<ChallengeOutput>, SolverError> {
    SolverSession::new(prepared)?.solve_batch(inputs)
}

impl SolverSession {
    pub fn new(prepared: &PreparedPlayer) -> Result<Self, SolverError> {
        let mut context = Context::default();
        context
            .runtime_limits_mut()
            .set_loop_iteration_limit(5_000_000);
        context.runtime_limits_mut().set_recursion_limit(256);
        context.runtime_limits_mut().set_stack_size_limit(4096);
        context
            .eval(Source::from_bytes(&prepared.code))
            .map_err(|error| SolverError::Setup(error.to_string()))?;
        Ok(Self { context })
    }

    pub fn solve_batch(
        &mut self,
        inputs: &[ChallengeInput],
    ) -> Result<Vec<ChallengeOutput>, SolverError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let input_count = inputs.len();
        let inputs_json = serde_json::to_string(inputs)
            .expect("challenge input contains only JSON-serializable strings");
        let runner = format!(
            r#"JSON.stringify((() => {{
const inputs = {inputs_json};
return inputs.map(input => {{
  const values = [];
  for (const candidate of globalThis.__wotoha_candidates) {{
    try {{
      const value = candidate(input);
      if (value && !values.some(existing => JSON.stringify(existing) === JSON.stringify(value))) {{
        values.push(value);
      }}
    }} catch (_) {{}}
  }}
  return values;
}});
}})())"#
        );
        let output = self
            .context
            .eval(Source::from_bytes(&runner))
            .map_err(|error| SolverError::Execute(error.to_string()))?;
        let output = output
            .as_string()
            .ok_or_else(|| SolverError::Execute("solver did not return a string".to_owned()))?
            .to_std_string_escaped();
        let solution_sets: Vec<Vec<ChallengeOutput>> =
            serde_json::from_str(&output).map_err(SolverError::InvalidOutput)?;
        if solution_sets.len() != input_count {
            return Err(SolverError::NoUniqueSolution);
        }
        solution_sets
            .into_iter()
            .map(|solutions| {
                if solutions.len() != 1 {
                    Err(SolverError::NoUniqueSolution)
                } else {
                    Ok(solutions
                        .into_iter()
                        .next()
                        .expect("one solution was checked"))
                }
            })
            .collect()
    }
}

impl PreparedPlayer {
    pub fn candidate_count(&self) -> usize {
        self.candidate_count
    }
}

fn player_body<'a>(program: &'a Program<'a>) -> Option<&'a FunctionBody<'a>> {
    program.body.iter().rev().find_map(|statement| {
        let Statement::ExpressionStatement(statement) = statement else {
            return None;
        };
        let Expression::CallExpression(call) = &statement.expression else {
            return None;
        };
        function_from_callee(&call.callee)
            .and_then(|function| function.body.as_ref())
            .map(AsRef::as_ref)
    })
}

fn function_from_callee<'a>(callee: &'a Expression<'a>) -> Option<&'a Function<'a>> {
    match callee {
        Expression::FunctionExpression(function) => Some(function),
        Expression::ParenthesizedExpression(expression) => {
            function_from_callee(&expression.expression)
        }
        Expression::StaticMemberExpression(member) => function_from_callee(&member.object),
        Expression::ComputedMemberExpression(member) => function_from_callee(&member.object),
        _ => None,
    }
}

fn challenge_candidates(statements: &[Statement<'_>], source: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for statement in statements {
        match statement {
            Statement::FunctionDeclaration(function) => {
                if function_contains_marker(function)
                    && let Some(id) = function.id.as_ref()
                {
                    candidates.push(id.name.to_string());
                }
            }
            Statement::ExpressionStatement(statement) => {
                let Expression::AssignmentExpression(assignment) = &statement.expression else {
                    continue;
                };
                if assignment.operator == AssignmentOperator::Assign
                    && let Expression::FunctionExpression(function) = &assignment.right
                    && function_contains_marker(function)
                    && let Ok(name) = source_for_span(source, assignment.left.span())
                {
                    candidates.push(name.to_owned());
                }
            }
            Statement::VariableDeclaration(declaration) => {
                for declarator in &declaration.declarations {
                    let Some(Expression::FunctionExpression(function)) = declarator.init.as_ref()
                    else {
                        continue;
                    };
                    if !function_contains_marker(function) {
                        continue;
                    }
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        candidates.push(id.name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let mut seen = HashSet::with_capacity(candidates.len());
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    candidates
}

fn function_contains_marker(function: &Function<'_>) -> bool {
    let Some(body) = function.body.as_ref() else {
        return false;
    };
    let mut detector = ChallengeMarkerDetector::default();
    detector.visit_function_body(body);
    detector.found
}

#[derive(Default)]
struct ChallengeMarkerDetector {
    found: bool,
}

impl<'a> Visit<'a> for ChallengeMarkerDetector {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if self.found {
            return;
        }
        let is_member_call = matches!(
            &call.callee,
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_)
        );
        let has_marker = call.arguments.len() == 2
            && matches!(
                &call.arguments[0],
                Argument::StringLiteral(value) if value.value == "alr"
            )
            && matches!(
                &call.arguments[1],
                Argument::StringLiteral(value) if value.value == "yes"
            );
        if is_member_call && has_marker {
            self.found = true;
            return;
        }
        walk::walk_call_expression(self, call);
    }
}

fn keep_player_statement(statement: &Statement<'_>) -> bool {
    match statement {
        Statement::ExpressionStatement(statement) => matches!(
            statement.expression,
            Expression::AssignmentExpression(_)
                | Expression::BooleanLiteral(_)
                | Expression::NullLiteral(_)
                | Expression::NumericLiteral(_)
                | Expression::BigIntLiteral(_)
                | Expression::StringLiteral(_)
        ),
        _ => true,
    }
}

fn is_window_alias(statement: &Statement<'_>, source: &str) -> bool {
    source_for_span(source, statement.span())
        .map(|value| {
            value
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect::<String>()
                .starts_with("varwindow=this")
        })
        .unwrap_or(false)
}

fn source_for_span(source: &str, span: Span) -> Result<&str, SolverError> {
    source
        .get(span.start as usize..span.end as usize)
        .ok_or(SolverError::Structure)
}

fn candidate_wrapper(candidate: &str) -> String {
    format!(
        r#"(__input) => {{
  const __url = ({candidate})(
    "https://youtube.com/watch?v=wotoha",
    "s",
    __input.signature === null ? undefined : encodeURIComponent(__input.signature)
  );
  if (__input.n !== null) __url.set("n", __input.n);
  const __proto = Object.getPrototypeOf(__url);
  const __keys = Object.keys(__proto).concat(Object.getOwnPropertyNames(__proto));
  for (const __key of __keys) {{
    if (!["constructor", "set", "get", "clone"].includes(__key)) {{
      __url[__key]();
      break;
    }}
  }}
  const __signature = __url.get("s");
  return {{
    signature: __signature ? decodeURIComponent(__signature) : null,
    n: __url.get("n") ?? null
  }};
}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER_FIXTURE: &str = r#"
var _player = {};
(function(g) {
  var window = this;
  function Param() { this.values = new Map(); }
  Param.prototype.set = function(key, value) { this.values.set(key, value); };
  Param.prototype.get = function(key) { return this.values.get(key); };
  Param.prototype.clone = function() { return this; };
  Param.prototype.transform = function() {
    const sig = this.values.get("s");
    if (sig) this.values.set("s", sig.split("").reverse().join(""));
    const n = this.values.get("n");
    if (n) this.values.set("n", n.slice(1) + n[0]);
  };
  function solve(a, b, c) {
    const value = new Param();
    value.set("alr", "yes");
    if (c) value.set("s", c);
    return value;
  }
})(_player);
"#;

    #[test]
    fn prepares_ast_identified_solver_candidate() {
        let prepared = prepare_player(PLAYER_FIXTURE).unwrap();
        assert_eq!(prepared.candidate_count(), 1);
    }

    #[test]
    fn marker_detection_does_not_depend_on_the_minified_method_name() {
        for marker in [
            r#"value.renamed("alr", "yes")"#,
            r#"value["renamed"]("alr", "yes")"#,
        ] {
            let source = PLAYER_FIXTURE.replace(r#"value.set("alr", "yes")"#, marker);
            assert_eq!(prepare_player(&source).unwrap().candidate_count(), 1);
        }
    }

    #[test]
    fn solves_signature_and_n_challenges() {
        let prepared = prepare_player(PLAYER_FIXTURE).unwrap();
        let output = solve(
            &prepared,
            &ChallengeInput {
                signature: Some("abcdef".to_owned()),
                n: Some("1234".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(
            output,
            ChallengeOutput {
                signature: Some("fedcba".to_owned()),
                n: Some("2341".to_owned()),
            }
        );
    }

    #[test]
    fn solves_multiple_challenges_in_one_player_evaluation() {
        let prepared = prepare_player(PLAYER_FIXTURE).unwrap();
        let output = solve_batch(
            &prepared,
            &[
                ChallengeInput {
                    signature: Some("abcdef".to_owned()),
                    n: Some("1234".to_owned()),
                },
                ChallengeInput {
                    signature: Some("xyz".to_owned()),
                    n: Some("789".to_owned()),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            output,
            vec![
                ChallengeOutput {
                    signature: Some("fedcba".to_owned()),
                    n: Some("2341".to_owned()),
                },
                ChallengeOutput {
                    signature: Some("zyx".to_owned()),
                    n: Some("897".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn reuses_prepared_player_session_across_batches() {
        let prepared = prepare_player(PLAYER_FIXTURE).unwrap();
        let mut session = SolverSession::new(&prepared).unwrap();
        for signature in ["first", "second"] {
            let output = session
                .solve_batch(&[ChallengeInput {
                    signature: Some(signature.to_owned()),
                    n: None,
                }])
                .unwrap();
            assert_eq!(
                output[0].signature.as_deref(),
                Some(signature.chars().rev().collect::<String>().as_str())
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires live YouTube access"]
    async fn solves_current_live_player() {
        let client = reqwest::Client::new();
        let player_url = match std::env::var("WOTOHA_YOUTUBE_PLAYER_JS_URL") {
            Ok(player_url) => player_url,
            Err(_) => {
                let html = client
                    .get("https://www.youtube.com/watch?v=H7HmzwI67ec&hl=en")
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                player_url_from_watch_html(&html).expect("watch page should expose Player URL")
            }
        };
        let source = client
            .get(player_url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        let prepared = prepare_player(&source).unwrap();
        eprintln!("candidate_count={}", prepared.candidate_count());
        let setup_started = std::time::Instant::now();
        let mut session = SolverSession::new(&prepared).unwrap();
        let setup_elapsed = setup_started.elapsed();
        let first_started = std::time::Instant::now();
        let inputs = [
            ChallengeInput {
                signature: Some("abcdefghijklmnopqrstuvwxyz".to_owned()),
                n: None,
            },
            ChallengeInput {
                signature: None,
                n: Some("1234567890abcdef".to_owned()),
            },
        ];
        let output = session.solve_batch(&inputs).unwrap();
        let first_elapsed = first_started.elapsed();
        assert_ne!(
            output[0].signature.as_deref(),
            inputs[0].signature.as_deref()
        );
        assert!(output[0].n.is_none());
        assert_ne!(output[1].n.as_deref(), inputs[1].n.as_deref());
        assert!(output[1].signature.is_none());
        let second_started = std::time::Instant::now();
        let second = session.solve_batch(&inputs).unwrap();
        let second_elapsed = second_started.elapsed();
        assert_eq!(second, output);
        eprintln!(
            "setup_ms={} first_ms={} second_ms={}",
            setup_elapsed.as_millis(),
            first_elapsed.as_millis(),
            second_elapsed.as_millis()
        );
    }

    fn player_url_from_watch_html(html: &str) -> Option<String> {
        for marker in [r#""jsUrl""#, r#""PLAYER_JS_URL""#] {
            if let Some((_, tail)) = html.split_once(marker) {
                let Some((_, encoded_tail)) = tail.split_once(':') else {
                    continue;
                };
                let Some(encoded) = encoded_tail
                    .trim_start()
                    .strip_prefix('"')
                    .and_then(|value| value.split_once('"').map(|(value, _)| value))
                else {
                    continue;
                };
                let Ok(decoded) = serde_json::from_str::<String>(&format!("\"{encoded}\"")) else {
                    continue;
                };
                if decoded.starts_with('/') && !decoded.starts_with("//") {
                    return Some(format!("https://www.youtube.com{decoded}"));
                }
                if decoded.starts_with("https://www.youtube.com/") {
                    return Some(decoded);
                }
            }
        }
        None
    }

    #[test]
    fn discovers_safe_player_urls_in_watch_html_variants() {
        let expected = "https://www.youtube.com/s/player/b81a9a58/player_ias.vflset/en_US/base.js";
        for html in [
            r#"{"jsUrl":"\/s\/player\/b81a9a58\/player_ias.vflset\/en_US\/base.js"}"#,
            r#"{"PLAYER_JS_URL" : "\/s\/player\/b81a9a58\/player_ias.vflset\/en_US\/base.js"}"#,
            r#"{"jsUrl":"\uD800","PLAYER_JS_URL":"\/s\/player\/b81a9a58\/player_ias.vflset\/en_US\/base.js"}"#,
        ] {
            assert_eq!(player_url_from_watch_html(html).as_deref(), Some(expected));
        }
        assert!(
            player_url_from_watch_html(
                r#"{"jsUrl":"\/\/attacker.example\/s\/player\/x\/base.js"}"#
            )
            .is_none()
        );
        assert!(
            player_url_from_watch_html(
                r#"{"jsUrl":"https:\/\/attacker.example\/s\/player\/x\/base.js"}"#
            )
            .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires live YouTube access to a historical Player"]
    async fn matches_official_ejs_golden_vectors() {
        const PLAYER: &str = "74edf1a3";
        const N_INPUT: &str = "IlLiA21ny7gqA2m4p37";
        const N_EXPECTED: &str = "9nRTxrbM1f0yHg";
        const SIG_INPUT: &str = "NJAJEij0EwRgIhAI0KExTgjfPk-MPM9MAdzyyPRt=BM8-XO5tm5hlMCSVpAiEAv7eP3CURqZNSPow8BXXAoazVoXgeMP7gH9BdylHCwgw=gwzz";
        const SIG_EXPECTED: &str = "NJAJEij0EwRgIhAI0KExTgjfPk-MPM9MAdzyyPRt=BM8-XO5tm5hzMCSVpAiEAv7eP3CURqZNSPow8BXXAoazVoXgeMP7gH9BdylHCwgw=gwzl";
        for variant in [
            "player_ias.vflset/en_US/base.js",
            "tv-player-ias.vflset/tv-player-ias.js",
        ] {
            let url = format!("https://www.youtube.com/s/player/{PLAYER}/{variant}");
            let source = reqwest::Client::new()
                .get(url)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            let prepared = prepare_player(&source).unwrap();
            let mut session = SolverSession::new(&prepared).unwrap();
            let outputs = session
                .solve_batch(&[
                    ChallengeInput {
                        signature: None,
                        n: Some(N_INPUT.to_owned()),
                    },
                    ChallengeInput {
                        signature: Some(SIG_INPUT.to_owned()),
                        n: None,
                    },
                ])
                .unwrap();
            assert_eq!(outputs[0].n.as_deref(), Some(N_EXPECTED));
            assert_eq!(outputs[1].signature.as_deref(), Some(SIG_EXPECTED));
        }
    }
}
