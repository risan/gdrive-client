//! A small command line around the library: log in, upload with verification, list, download, delete.
//!
//! ```text
//! gdrive --store <path> [--endpoint <url>] [--chunk-mib <n>] [--deadline-secs <n>] [--print-peak-rss] <command>
//!
//! login <client-id>                 device flow; the client secret comes from GDRIVE_CLIENT_SECRET
//! import-rclone <config> <section>  one-time import of a drive section of an rclone config
//! set-credentials <client-id>       GDRIVE_CLIENT_SECRET and GDRIVE_REFRESH_TOKEN
//! upload <local> <folder-path>      new file, or new content for the file of that name; exit 1 if not verified
//! list <folder-path>
//! download <file-id> <local>
//! delete <file-id>
//! trash <file-id>
//! ```
//!
//! Secrets are read from the environment so they never show in `ps` or in shell history.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use gdrive_client::{Client, Credentials, DeviceLogin, Endpoints, Options};

const USAGE: &str = "usage: gdrive --store <path> [--endpoint <url>] [--chunk-mib <n>] [--deadline-secs <n>] \
[--print-peak-rss] <login|import-rclone|set-credentials|upload|list|download|delete|trash> <arguments>";

struct Arguments {
    store: PathBuf,
    endpoint: Option<String>,
    chunk_mib: Option<usize>,
    deadline_secs: Option<u64>,
    print_peak_rss: bool,
    command: Vec<String>,
}

fn parse_arguments(mut arguments: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut store = None;
    let mut endpoint = None;
    let mut chunk_mib = None;
    let mut deadline_secs = None;
    let mut print_peak_rss = false;
    let mut command = Vec::new();

    while let Some(argument) = arguments.next() {
        let mut value = |name: &str| arguments.next().ok_or_else(|| format!("{name} needs a value"));

        match argument.as_str() {
            "--store" => store = Some(PathBuf::from(value("--store")?)),
            "--endpoint" => endpoint = Some(value("--endpoint")?),
            "--chunk-mib" => {
                chunk_mib = Some(
                    value("--chunk-mib")?
                        .parse()
                        .map_err(|_| "--chunk-mib needs a number")?,
                );
            }
            "--deadline-secs" => {
                deadline_secs = Some(
                    value("--deadline-secs")?
                        .parse()
                        .map_err(|_| "--deadline-secs needs a number")?,
                );
            }
            "--print-peak-rss" => print_peak_rss = true,
            _ => command.push(argument),
        }
    }

    Ok(Arguments {
        store: store.ok_or("--store is required")?,
        endpoint,
        chunk_mib,
        deadline_secs,
        print_peak_rss,
        command,
    })
}

fn options(arguments: &Arguments) -> Options {
    let mut options = Options::default();

    if let Some(endpoint) = &arguments.endpoint {
        options.endpoints = Endpoints::local(endpoint.as_str());
    }

    if let Some(chunk_mib) = arguments.chunk_mib {
        options.chunk_size = chunk_mib * 1024 * 1024;
    }

    if let Some(seconds) = arguments.deadline_secs {
        options.deadline = Duration::from_secs(seconds);
    }

    options
}

fn secret_from_environment(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} must be set"))
}

fn open(arguments: &Arguments) -> Result<Client, String> {
    Client::open(&arguments.store, options(arguments)).map_err(|error| error.to_string())
}

fn run(arguments: &Arguments) -> Result<ExitCode, String> {
    let command: Vec<&str> = arguments.command.iter().map(String::as_str).collect();
    let fail = |error: gdrive_client::Error| error.to_string();
    let client = match command.as_slice() {
        ["login", client_id] => {
            let secret = secret_from_environment("GDRIVE_CLIENT_SECRET")?;
            let login = DeviceLogin::start(client_id, &secret, &options(arguments)).map_err(fail)?;

            println!(
                "Open {} and enter the code {}",
                login.verification_url(),
                login.user_code()
            );

            let credentials = login.wait().map_err(fail)?;

            println!("logged in");
            return create_store(arguments, credentials);
        }
        ["import-rclone", config, section] => {
            let credentials = Credentials::from_rclone_config(Path::new(config), section).map_err(fail)?;

            return create_store(arguments, credentials);
        }
        ["set-credentials", client_id] => {
            let credentials = Credentials::new(
                *client_id,
                secret_from_environment("GDRIVE_CLIENT_SECRET")?,
                secret_from_environment("GDRIVE_REFRESH_TOKEN")?,
            );

            return create_store(arguments, credentials);
        }
        _ => open(arguments)?,
    };
    let code = match command.as_slice() {
        ["upload", local, folder_path] => upload(&client, Path::new(local), folder_path)?,
        ["list", folder_path] => {
            let folder = client.folder_id(folder_path).map_err(fail)?;

            for file in client.list_files(&folder).map_err(fail)? {
                println!(
                    "{} {} {} {}",
                    file.id,
                    file.size.map_or_else(|| "-".to_string(), |size| size.to_string()),
                    file.md5.as_deref().unwrap_or("-"),
                    file.name
                );
            }

            ExitCode::SUCCESS
        }
        ["download", file_id, local] => {
            let download = client.download(file_id, Path::new(local)).map_err(fail)?;

            println!("downloaded {} bytes, md5 {}", download.size, download.md5);
            ExitCode::SUCCESS
        }
        ["delete", file_id] => {
            client.delete(file_id).map_err(fail)?;
            ExitCode::SUCCESS
        }
        ["trash", file_id] => {
            client.trash(file_id).map_err(fail)?;
            ExitCode::SUCCESS
        }
        _ => return Err(USAGE.to_string()),
    };

    report_store_failure(&client);
    Ok(code)
}

fn create_store(arguments: &Arguments, credentials: Credentials) -> Result<ExitCode, String> {
    let client =
        Client::create(&arguments.store, credentials, options(arguments)).map_err(|error| error.to_string())?;

    report_store_failure(&client);
    Ok(ExitCode::SUCCESS)
}

fn upload(client: &Client, local: &Path, folder_path: &str) -> Result<ExitCode, String> {
    let fail = |error: gdrive_client::Error| error.to_string();
    let name = local
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("the local path has no file name")?;
    let folder = client.ensure_folder(folder_path).map_err(fail)?;
    let existing = client.find_files(&folder, name).map_err(fail)?;
    let upload = match existing.as_slice() {
        [] => client.upload_new(local, &folder, name),
        [file] => client.upload_replace(local, &file.id),
        several => return Err(format!("{} files named {name} in {folder_path}", several.len())),
    }
    .map_err(fail)?;

    println!(
        "id {} size {} md5 {}",
        upload.file.id,
        upload
            .file
            .size
            .map_or_else(|| "-".to_string(), |size| size.to_string()),
        upload.file.md5.as_deref().unwrap_or("-")
    );

    if upload.verified() {
        println!("verified");
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "not verified (local size {}, local md5 {}, local file changed: {})",
            upload.local_size, upload.local_md5, upload.local_file_changed
        );
        Ok(ExitCode::FAILURE)
    }
}

fn report_store_failure(client: &Client) {
    if let Some(error) = client.take_store_failure() {
        eprintln!("warning: token store not written: {error}");
    }
}

/// `VmHWM`, the process's peak resident set size in KiB.
fn peak_rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;

    line.split_whitespace().nth(1)?.parse().ok()
}

fn main() -> ExitCode {
    let arguments = match parse_arguments(env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let code = match run(&arguments) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    };

    if arguments.print_peak_rss {
        println!("VmHWM_KiB {}", peak_rss_kib().unwrap_or(0));
    }

    code
}
