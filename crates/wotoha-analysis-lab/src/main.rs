use std::{env, error::Error, path::PathBuf};

use wotoha_analysis_lab::{
    AnalyzerMode, EvaluationOptions, ExternalObservationDocument, REPORT_SCHEMA_VERSION,
    TempoExperimentReportDocument, evaluate_exported_manifest, evaluate_manifest, export_blackbox,
    generate_default_manifest, load_manifest, package_blackbox, run_classical_tempo_research,
    run_fixed_tempo_consensus_research, run_fixed_tempo_consensus_synthetic_research,
    run_ground_truth_research, run_independent_positive_corpus_research, run_real_song_research,
    run_realistic_corpus_research, run_tempo_advisor_research, run_tempo_ambiguity_research,
    run_tempo_conservative_shadow_research, run_tempo_shadow_followup, verify_blackbox_package,
    write_json,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("analysis_lab: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or_else(usage)?;
    match command.as_str() {
        "generate" => {
            let (output, seed) = parse_generate_args(&mut args)?;
            let manifest = generate_default_manifest(seed)?;
            write_json(&output, &manifest)?;
            println!(
                "generated {} fixtures at {}",
                manifest.fixtures.len(),
                output.display()
            );
        }
        "export-blackbox" => {
            let (output, seed) = parse_generate_args(&mut args)?;
            let manifest = export_blackbox(&output, seed)?;
            println!(
                "exported {} WAV fixtures at {}",
                manifest.fixtures.len(),
                output.display()
            );
        }
        "package-blackbox" => {
            let (input, output) = parse_package_args(&mut args)?;
            reject_unknown(args)?;
            package_blackbox(&input, &output)?;
            println!("packaged black-box corpus at {}", output.display());
        }
        "verify-blackbox" => {
            let path = args
                .next()
                .map(PathBuf::from)
                .ok_or("verify-blackbox requires a ZIP path")?;
            reject_unknown(args)?;
            verify_blackbox_package(&path)?;
            println!("verified black-box package {}", path.display());
        }
        "evaluate" | "baseline" => {
            let evaluate = parse_evaluate_args(command == "baseline", &mut args)?;
            reject_unknown(args)?;
            let manifest = if let Some(path) = evaluate.manifest {
                load_manifest(&path)?
            } else {
                generate_default_manifest(0x57_4f_54_4f_48_41)?
            };
            let external = evaluate
                .external
                .map(|path| ExternalObservationDocument::load(&path))
                .transpose()?;
            let report = evaluate_manifest(
                &manifest,
                external.as_ref(),
                EvaluationOptions {
                    mode: evaluate.mode,
                    split: evaluate.split.unwrap_or_else(|| manifest.split.clone()),
                    source_commit: env::var("WOTOHA_SOURCE_COMMIT").ok(),
                    include_backend_comparison: true,
                },
            )?;
            write_json(&evaluate.output, &report)?;
            println!("{}", report.human_summary());
            println!("report={}", evaluate.output.display());
        }
        "evaluate-exported" => {
            let evaluate = parse_exported_args(&mut args)?;
            reject_unknown(args)?;
            let external = evaluate
                .external
                .map(|path| ExternalObservationDocument::load(&path))
                .transpose()?;
            let report = evaluate_exported_manifest(
                &evaluate.manifest,
                &evaluate.audio_root,
                external.as_ref(),
                EvaluationOptions {
                    mode: evaluate.mode,
                    split: evaluate.split.unwrap_or_else(|| "development".into()),
                    source_commit: env::var("WOTOHA_SOURCE_COMMIT").ok(),
                    include_backend_comparison: true,
                },
            )?;
            write_json(&evaluate.output, &report)?;
            println!("{}", report.human_summary());
            println!("report={}", evaluate.output.display());
        }
        "research-tempo" => {
            let evaluate = parse_evaluate_args(false, &mut args)?;
            reject_unknown(args)?;
            let report = if let Some(audio_root) = evaluate.audio_root {
                let manifest = evaluate
                    .manifest
                    .ok_or("research-tempo --audio-root requires --manifest")?;
                evaluate_exported_manifest(
                    &manifest,
                    &audio_root,
                    None,
                    EvaluationOptions {
                        mode: evaluate.mode,
                        split: evaluate.split.unwrap_or_else(|| "development".into()),
                        source_commit: env::var("WOTOHA_SOURCE_COMMIT").ok(),
                        include_backend_comparison: true,
                    },
                )?
            } else {
                let manifest = if let Some(path) = evaluate.manifest {
                    load_manifest(&path)?
                } else {
                    generate_default_manifest(0x57_4f_54_4f_48_41)?
                };
                evaluate_manifest(
                    &manifest,
                    None,
                    EvaluationOptions {
                        mode: evaluate.mode,
                        split: evaluate.split.unwrap_or_else(|| manifest.split.clone()),
                        source_commit: env::var("WOTOHA_SOURCE_COMMIT").ok(),
                        include_backend_comparison: true,
                    },
                )?
            };
            write_json(
                &evaluate.output,
                &TempoExperimentReportDocument {
                    schema_version: REPORT_SCHEMA_VERSION,
                    evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
                    split: report.split,
                    analyzer_mode: report.analyzer_mode,
                    source_commit: report.source_commit,
                    experiment: report.tempo_experiment,
                },
            )?;
            println!("tempo experiment report={}", evaluate.output.display());
        }
        "research-pass" => {
            let evaluate = parse_exported_args(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-pass requires WOTOHA_SOURCE_COMMIT")?;
            let summary = run_ground_truth_research(
                &evaluate.manifest,
                &evaluate.audio_root,
                &evaluate.output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "research pass complete: {} fixtures, output={}",
                summary.fixture_count,
                evaluate.output.display()
            );
        }
        "research-tempo-advisor" => {
            let evaluate = parse_exported_args(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-advisor requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_tempo_advisor_research(
                &evaluate.manifest,
                &evaluate.audio_root,
                &evaluate.output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "tempo advisor research complete: {} scalar fixtures, output={}",
                report.scalar_tempo_fixture_count,
                evaluate.output.display()
            );
        }
        "research-classical-tempo" => {
            let evaluate = parse_exported_args(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-classical-tempo requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_classical_tempo_research(
                &evaluate.manifest,
                &evaluate.audio_root,
                &evaluate.output,
                source_commit,
            )?;
            println!(
                "classical tempo research complete: {} fixtures, output={}",
                report.corpus.fixture_count,
                evaluate.output.display()
            );
        }
        "research-tempo-ambiguity" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-ambiguity requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_tempo_ambiguity_research(
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "tempo ambiguity research complete: {} fixtures, output={}",
                report.benchmark.fixture_count,
                output.display()
            );
        }
        "research-tempo-shadow" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-shadow requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_tempo_shadow_followup(
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "tempo shadow follow-up complete: {} fixtures, output={}",
                report.fixture_count,
                output.display()
            );
        }
        "research-tempo-conservative-shadow" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-conservative-shadow requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_tempo_conservative_shadow_research(
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "conservative tempo shadow research complete: {} scalar fixtures, output={}",
                report.scalar_tempo_fixture_count,
                output.display()
            );
        }
        "research-tempo-realistic-shadow" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-realistic-shadow requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_realistic_corpus_research(
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "realistic shadow research complete: {} fixtures, output={}",
                report.fixture_count,
                output.display()
            );
        }
        "research-tempo-independent-positive" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-tempo-independent-positive requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_independent_positive_corpus_research(
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "independent positive research complete: {} fixtures, {} pairs, output={}",
                report.fixture_count,
                report.pair_count,
                output.display()
            );
        }
        "research-real-songs" => {
            let (audio_root, output) = parse_real_song_args(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT")
                .map_err(|_| "research-real-songs requires WOTOHA_SOURCE_COMMIT")?;
            let report = run_real_song_research(
                &audio_root,
                &output,
                source_commit,
                env::var("WOTOHA_STARTING_COMMIT").ok(),
            )?;
            println!(
                "real-song research complete: {} tracks, output={}",
                report.track_count,
                output.display()
            );
        }
        "research-fixed-tempo-consensus" => {
            let (input, output) = parse_fixed_tempo_consensus_args(&mut args)?;
            reject_unknown(args)?;
            let report = run_fixed_tempo_consensus_research(&input, &output)?;
            println!(
                "fixed-tempo consensus research complete: {} tracks, output={}",
                report.summary.tracks,
                output.display()
            );
        }
        "research-fixed-tempo-consensus-synthetic" => {
            let output = parse_output_dir(&mut args)?;
            reject_unknown(args)?;
            let source_commit = env::var("WOTOHA_SOURCE_COMMIT").map_err(
                |_| "research-fixed-tempo-consensus-synthetic requires WOTOHA_SOURCE_COMMIT",
            )?;
            let report = run_fixed_tempo_consensus_synthetic_research(&output, source_commit)?;
            println!(
                "fixed-tempo synthetic E2E complete: {} fixtures, false_confident={}, output={}",
                report.cases.len(),
                report.false_confident_selections,
                output.display()
            );
        }
        "--help" | "-h" => println!("{}", usage()),
        _ => return Err(usage().into()),
    }
    Ok(())
}

struct EvaluateArgs {
    manifest: Option<PathBuf>,
    audio_root: Option<PathBuf>,
    output: PathBuf,
    external: Option<PathBuf>,
    mode: AnalyzerMode,
    split: Option<String>,
}

struct ExportedArgs {
    manifest: PathBuf,
    audio_root: PathBuf,
    output: PathBuf,
    external: Option<PathBuf>,
    mode: AnalyzerMode,
    split: Option<String>,
}

fn parse_exported_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<ExportedArgs, Box<dyn Error + Send + Sync>> {
    let mut manifest = None;
    let mut audio_root = None;
    let mut output = PathBuf::from("analysis-lab-exported-report.json");
    let mut external = None;
    let mut mode = AnalyzerMode::Hybrid;
    let mut split = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--manifest" => manifest = Some(next_path(args, "--manifest")?),
            "--audio-root" => audio_root = Some(next_path(args, "--audio-root")?),
            "--external-observations" => {
                external = Some(next_path(args, "--external-observations")?)
            }
            "--report" => output = next_path(args, "--report")?,
            "--output" => output = next_path(args, "--output")?,
            "--mode" => mode = next_value(args, "--mode")?.parse()?,
            "--split" => split = Some(next_value(args, "--split")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    Ok(ExportedArgs {
        manifest: manifest.ok_or("--manifest is required")?,
        audio_root: audio_root.ok_or("--audio-root is required")?,
        output,
        external,
        mode,
        split,
    })
}

fn parse_evaluate_args(
    baseline: bool,
    args: &mut impl Iterator<Item = String>,
) -> Result<EvaluateArgs, Box<dyn Error + Send + Sync>> {
    let mut manifest = None;
    let mut audio_root = None;
    let mut output = PathBuf::from("analysis-lab-report.json");
    let mut external = None;
    let mut mode = AnalyzerMode::Hybrid;
    let mut split = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--manifest" => manifest = Some(next_path(args, "--manifest")?),
            "--audio-root" => audio_root = Some(next_path(args, "--audio-root")?),
            "--external-observations" => {
                external = Some(next_path(args, "--external-observations")?)
            }
            "--report" => output = next_path(args, "--report")?,
            "--mode" => mode = next_value(args, "--mode")?.parse()?,
            "--split" => split = Some(next_value(args, "--split")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    if baseline && manifest.is_some() {
        return Err("baseline does not accept --manifest; use evaluate".into());
    }
    Ok(EvaluateArgs {
        manifest,
        audio_root,
        output,
        external,
        mode,
        split,
    })
}

fn parse_generate_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(PathBuf, u64), Box<dyn Error + Send + Sync>> {
    let mut output = None;
    let mut seed = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--output" => {
                if output.is_some() {
                    return Err("--output may be supplied once".into());
                }
                output = Some(next_path(args, "--output")?);
            }
            "--seed" => {
                if seed.is_some() {
                    return Err("--seed may be supplied once".into());
                }
                seed = Some(next_value(args, "--seed")?.parse()?);
            }
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    Ok((
        output.ok_or("--output is required")?,
        seed.unwrap_or(0x57_4f_54_4f_48_41),
    ))
}

fn parse_package_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(PathBuf, PathBuf), Box<dyn Error + Send + Sync>> {
    let mut input = None;
    let mut output = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--input" => input = Some(next_path(args, "--input")?),
            "--output" => output = Some(next_path(args, "--output")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    Ok((
        input.ok_or("--input is required")?,
        output.ok_or("--output is required")?,
    ))
}

fn parse_output_dir(
    args: &mut impl Iterator<Item = String>,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let mut output = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--output" => output = Some(next_path(args, "--output")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    output.ok_or_else(|| "--output is required".into())
}

fn parse_real_song_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(PathBuf, PathBuf), Box<dyn Error + Send + Sync>> {
    let mut audio_root = None;
    let mut output = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--audio-root" => audio_root = Some(next_path(args, "--audio-root")?),
            "--output" => output = Some(next_path(args, "--output")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    Ok((
        audio_root.ok_or("--audio-root is required")?,
        output.ok_or("--output is required")?,
    ))
}

fn parse_fixed_tempo_consensus_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(PathBuf, PathBuf), Box<dyn Error + Send + Sync>> {
    let mut input = None;
    let mut output = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--input" => input = Some(next_path(args, "--input")?),
            "--output" => output = Some(next_path(args, "--output")?),
            "--help" | "-h" => return Err(usage().into()),
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    Ok((
        input.ok_or("--input is required")?,
        output.ok_or("--output is required")?,
    ))
}

fn next_path(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    Ok(PathBuf::from(next_value(args, flag)?))
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value").into())
}

fn reject_unknown(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Some(argument) = args.next() {
        return Err(format!("unknown option: {argument}").into());
    }
    Ok(())
}

fn usage() -> &'static str {
    "usage: analysis_lab generate --output PATH [--seed N]\n       analysis_lab export-blackbox --output DIRECTORY [--seed N]\n       analysis_lab package-blackbox --input DIRECTORY --output ZIP\n       analysis_lab verify-blackbox ZIP\n       analysis_lab evaluate --manifest PATH [--external-observations PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]\n       analysis_lab evaluate-exported --manifest PATH --audio-root DIRECTORY [--external-observations PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]\n       analysis_lab baseline [--external-observations PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]\n       analysis_lab research-tempo [--manifest PATH] [--audio-root PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]\n       analysis_lab research-pass --manifest PATH --audio-root DIRECTORY --output DIRECTORY\n       analysis_lab research-tempo-advisor --manifest PATH --audio-root DIRECTORY --output DIRECTORY\n       analysis_lab research-classical-tempo --manifest PATH --audio-root DIRECTORY --output DIRECTORY\n       analysis_lab research-tempo-ambiguity --output DIRECTORY\n       analysis_lab research-tempo-realistic-shadow --output DIRECTORY\n       analysis_lab research-real-songs --audio-root DIRECTORY --output DIRECTORY\n       analysis_lab research-fixed-tempo-consensus --input JSON --output DIRECTORY"
}
