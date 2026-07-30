use crate::OutputDestination;
use glua_code_analysis::EmmyLuaAnalysis;
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, BufWriter, Write},
};

mod export;
mod json_types;

pub fn generate_json(
    analysis: &EmmyLuaAnalysis,
    output: OutputDestination,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = analysis.compilation.get_db();

    let output = match output {
        OutputDestination::File(output)
            if output
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json")) =>
        {
            if let Some(parent) = output.parent()
                && !parent.exists()
            {
                log::info!("Creating output directory: {:?}", parent);
                std::fs::create_dir_all(parent)?;
            }

            OutputDestination::File(output)
        }
        OutputDestination::File(output) => {
            if !output.exists() {
                log::info!("Creating output directory: {:?}", output);
                std::fs::create_dir_all(&output)?;
            }

            OutputDestination::File(output.join("doc.json"))
        }
        OutputDestination::Stdout => OutputDestination::Stdout,
    };

    let data = export::export(db)?;

    match output {
        OutputDestination::Stdout => {
            let stdout = io::stdout();
            let mut writer = BufWriter::new(stdout.lock());
            serde_json::to_writer_pretty(&mut writer, &data)?;
            writeln!(writer)?;
            writer.flush()?;
        }
        OutputDestination::File(json_path) => {
            log::info!("Writing JSON to: {:?}", json_path);
            let mut writer = BufWriter::new(File::create(&json_path)?);
            serde_json::to_writer_pretty(&mut writer, &data)?;
            writeln!(writer)?;
            writer.flush()?;
            eprintln!("Documentation JSON exported to {:?}", json_path);
        }
    }

    Ok(())
}
