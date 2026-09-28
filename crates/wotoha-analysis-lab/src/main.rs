use std::{env, error::Error, path::PathBuf};

use wotoha_analysis_lab::{
    AnalyzerMode, EvaluationOptions, ExternalObservationDocument, evaluate_manifest,
    generate_default_manifest, load_manifest, write_json,
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
                },
            )?;
            write_json(&evaluate.output, &report)?;
            println!("{}", report.human_summary());
            println!("report={}", evaluate.output.display());
        }
        "--help" | "-h" => println!("{}", usage()),
        _ => return Err(usage().into()),
    }
    Ok(())
}

struct EvaluateArgs {
    manifest: Option<PathBuf>,
    output: PathBuf,
    external: Option<PathBuf>,
    mode: AnalyzerMode,
    split: Option<String>,
}

fn parse_evaluate_args(
    baseline: bool,
    args: &mut impl Iterator<Item = String>,
) -> Result<EvaluateArgs, Box<dyn Error + Send + Sync>> {
    let mut manifest = None;
    let mut output = PathBuf::from("analysis-lab-report.json");
    let mut external = None;
    let mut mode = AnalyzerMode::Hybrid;
    let mut split = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--manifest" => manifest = Some(next_path(args, "--manifest")?),
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
    "usage: analysis_lab generate --output PATH [--seed N]\n       analysis_lab evaluate --manifest PATH [--external-observations PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]\n       analysis_lab baseline [--external-observations PATH] [--report PATH] [--mode hybrid|classical] [--split NAME]"
}
