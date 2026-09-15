use std::env;

use serde::Serialize;

use crate::{cli::DoctorArgs, error::RamizError, fsclone, output};

#[derive(Serialize)]
struct DoctorData {
    path: output::EncodedPath,
    available: bool,
    backend: Option<&'static str>,
    reason: Option<String>,
}

pub fn run(args: DoctorArgs) -> Result<(), RamizError> {
    let requested = args.path.unwrap_or(
        env::current_dir()
            .map_err(|error| RamizError::new("current_directory", error.to_string(), args.json))?,
    );
    let directory = fsclone::existing_directory(&requested)
        .map_err(|error| RamizError::new("invalid_probe_path", error, args.json))?;
    let (backend, reason) = match fsclone::probe(&directory) {
        Ok(backend) => (Some(backend), None),
        Err(reason) => (None, Some(reason)),
    };
    let data = DoctorData {
        path: output::EncodedPath::new(&directory),
        available: backend.is_some(),
        backend: backend.map(|backend| backend.as_str()),
        reason,
    };
    if args.json {
        output::success("doctor", data, &[]);
    } else if let Some(backend) = data.backend {
        println!(
            "{}: copy-on-write available via {backend}",
            directory.display()
        );
    } else {
        println!(
            "{}: copy-on-write unavailable ({})",
            directory.display(),
            data.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    Ok(())
}
