// SPDX-License-Identifier: MIT

use browser_session::capture::{
    capture, CaptureOptions, CaptureResult, DEFAULT_CDP_PORT, DEFAULT_TIMEOUT_SECS,
};
use browser_session::error::{Result, SessionError};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Capture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    command: Command,
    url: Option<String>,
    cookie_names: Vec<String>,
    output: Option<PathBuf>,
    browser: Option<String>,
    profile_dir: Option<PathBuf>,
    port: u16,
    timeout_secs: u64,
    proxy: Option<String>,
    script_file: Option<PathBuf>,
}

#[derive(Serialize)]
struct SuccessEnvelope<T> {
    ok: bool,
    schema: u32,
    protocol: u32,
    version: &'static str,
    data: T,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ErrorPayload {
    code: &'static str,
    message: String,
    retryable: bool,
    #[serde(rename = "auth-required")]
    auth_required: bool,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    ok: bool,
    schema: u32,
    protocol: u32,
    version: &'static str,
    error: ErrorPayload,
    warnings: Vec<String>,
}

fn main() {
    match run(env::args().skip(1)) {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            println!("{}", encode_error(&error));
            process::exit(1);
        }
    }
}

fn encode_error(error: &SessionError) -> String {
    serde_json::to_string(&ErrorEnvelope {
        ok: false,
        schema: browser_session::capture::CAPTURE_SCHEMA_VERSION,
        protocol: PROTOCOL_VERSION,
        version: VERSION,
        error: ErrorPayload {
            code: error.code,
            message: error.message.clone(),
            retryable: error.retryable,
            auth_required: error.auth_required,
        },
        warnings: Vec::new(),
    })
    .expect("error envelope is serializable")
}

fn run<I>(arguments: I) -> Result<String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let options = parse_args(arguments)?;
    let data = match options.command {
        Command::Help => json!({"usage": usage()}),
        Command::Version => json!({
            "schema": browser_session::capture::CAPTURE_SCHEMA_VERSION,
            "protocol": PROTOCOL_VERSION,
            "version": VERSION
        }),
        Command::Capture => {
            let capture = capture(&CaptureOptions {
                url: options
                    .url
                    .expect("capture URL was validated by parse_args"),
                cookie_names: options.cookie_names,
                output: options
                    .output
                    .expect("capture output was validated by parse_args"),
                browser: options.browser,
                profile_dir: options.profile_dir,
                port: options.port,
                timeout: Duration::from_secs(options.timeout_secs),
                proxy: options.proxy,
                script_file: options.script_file,
            })?;
            capture_result_json(capture)
        }
    };
    serde_json::to_string(&SuccessEnvelope {
        ok: true,
        schema: browser_session::capture::CAPTURE_SCHEMA_VERSION,
        protocol: PROTOCOL_VERSION,
        version: VERSION,
        data,
        warnings: Vec::new(),
    })
    .map_err(|error| SessionError::helper_failure(format!("cannot encode response: {error}")))
}

fn capture_result_json(result: CaptureResult) -> serde_json::Value {
    json!({
        "browser": result.browser,
        "output": result.output,
        "url": result.url,
        "cookies": result.cookies,
        "page-captured": result.page_captured
    })
}

fn parse_args<I>(arguments: I) -> Result<Options>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let args = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.is_empty() {
        return Ok(Options {
            command: Command::Help,
            url: None,
            cookie_names: Vec::new(),
            output: None,
            browser: None,
            profile_dir: None,
            port: DEFAULT_CDP_PORT,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            proxy: None,
            script_file: None,
        });
    }
    let command = match args[0].as_str() {
        "help" | "--help" | "-h" => Command::Help,
        "version" | "--version" => Command::Version,
        "capture" => Command::Capture,
        other => {
            return Err(SessionError::invalid_request(format!(
                "unknown command `{other}`"
            )))
        }
    };
    if !matches!(command, Command::Capture) {
        if args.len() > 1 {
            return Err(SessionError::invalid_request(
                "help and version do not accept options",
            ));
        }
        return Ok(Options {
            command,
            url: None,
            cookie_names: Vec::new(),
            output: None,
            browser: None,
            profile_dir: None,
            port: DEFAULT_CDP_PORT,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            proxy: None,
            script_file: None,
        });
    }

    let mut url = None;
    let mut cookie_names = Vec::new();
    let mut output = None;
    let mut browser = None;
    let mut profile_dir = None;
    let mut port = DEFAULT_CDP_PORT;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut proxy = None;
    let mut script_file = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--url" => url = Some(option_value(&args, &mut index, "url")?),
            "--cookie" => cookie_names.push(option_value(&args, &mut index, "cookie")?),
            "--output" => output = Some(PathBuf::from(option_value(&args, &mut index, "output")?)),
            "--browser" => browser = Some(option_value(&args, &mut index, "browser")?),
            "--profile-dir" => {
                profile_dir = Some(PathBuf::from(option_value(
                    &args,
                    &mut index,
                    "profile-dir",
                )?))
            }
            "--port" => {
                let value = option_value(&args, &mut index, "port")?;
                port = value.parse().map_err(|_| {
                    SessionError::invalid_request(format!("invalid port `{value}`"))
                })?;
            }
            "--timeout-secs" => {
                let value = option_value(&args, &mut index, "timeout-secs")?;
                timeout_secs = value.parse().map_err(|_| {
                    SessionError::invalid_request(format!("invalid timeout `{value}`"))
                })?;
            }
            "--proxy" => proxy = Some(option_value(&args, &mut index, "proxy")?),
            "--script-file" => {
                script_file = Some(PathBuf::from(option_value(
                    &args,
                    &mut index,
                    "script-file",
                )?))
            }
            other => {
                return Err(SessionError::invalid_request(format!(
                    "unknown capture option `{other}`"
                )))
            }
        }
        index += 1;
    }

    let url = url.ok_or_else(|| SessionError::invalid_request("missing --url URL"))?;
    let output = output.ok_or_else(|| SessionError::invalid_request("missing --output FILE"))?;
    if cookie_names.is_empty() {
        return Err(SessionError::invalid_request(
            "capture requires at least one --cookie NAME",
        ));
    }
    Ok(Options {
        command,
        url: Some(url),
        cookie_names,
        output: Some(output),
        browser,
        profile_dir,
        port,
        timeout_secs,
        proxy,
        script_file,
    })
}

fn option_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| SessionError::invalid_request(format!("missing value for --{name}")))
}

fn usage() -> String {
    [
        "usage:",
        "  browser-session version",
        "  browser-session capture --url URL --cookie NAME... --output FILE [options]",
        "",
        "capture options:",
        "  --browser NAME|PATH       Chromium-compatible browser",
        "  --profile-dir DIR         Chromium user-data directory",
        "  --port N                  local DevTools port (default 29317)",
        "  --timeout-secs N          wait timeout (default 180)",
        "  --proxy URL               proxy for a browser started by the helper",
        "  --script-file FILE        provider-owned JavaScript expression",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capture_options() {
        let options = parse_args([
            "capture",
            "--url",
            "https://x.com",
            "--cookie",
            "auth_token",
            "--cookie",
            "ct0",
            "--output",
            "/tmp/session.json",
            "--browser",
            "chrome",
            "--profile-dir",
            "/tmp/profile",
            "--port",
            "29999",
            "--timeout-secs",
            "12",
        ])
        .unwrap();
        assert_eq!(options.command, Command::Capture);
        assert_eq!(options.cookie_names, vec!["auth_token", "ct0"]);
        assert_eq!(options.port, 29999);
        assert_eq!(options.timeout_secs, 12);
    }

    #[test]
    fn rejects_capture_without_cookies() {
        let error =
            parse_args(["capture", "--url", "https://x.com", "--output", "/tmp/a"]).unwrap_err();
        assert!(error.message.contains("--cookie"));
    }

    #[test]
    fn success_envelope_does_not_contain_cookie_values() {
        let output = capture_result_json(CaptureResult {
            browser: "chrome".to_string(),
            output: PathBuf::from("/tmp/session.json"),
            url: "https://x.com".to_string(),
            cookies: vec!["auth_token".to_string()],
            page_captured: false,
        });
        let text = serde_json::to_string(&output).unwrap();
        assert!(!text.contains("cookie-value"));
        assert!(text.contains("auth_token"));
    }
}
