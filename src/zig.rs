use std::{fs, path::Path};
use zed_extension_api::{
    self as zed,
    http_client::{HttpMethod, HttpRequest, RedirectPolicy},
    process::Command,
    serde_json, settings::LspSettings, LanguageServerId, Result,
};

const ZIG_TEST_EXE_BASENAME: &str = "zig_test";

/// The zigtools releases API endpoint for selecting a ZLS build that is
/// compatible with a given Zig version.
const ZLS_RELEASE_API: &str = "https://releases.zigtools.org/v1/zls/select-version";

struct ZigExtension {
    cached_binary: Option<(String, String)>, // (zls_version, path)
}

#[derive(Clone)]
struct ZlsBinary {
    path: String,
    args: Option<Vec<String>>,
    environment: Option<Vec<(String, String)>>,
}

struct ZlsRelease {
    version: String,
    download_url: String,
}

/// Extracts the `minimum_zig_version` field from a `build.zig.zon` file.
fn parse_minimum_zig_version(content: &str) -> Option<String> {
    let key = ".minimum_zig_version";
    let pos = content.find(key)?;
    let after_key = &content[pos + key.len()..];
    let after_eq = &after_key[after_key.find('=')? + 1..];
    let after_quote = &after_eq[after_eq.find('"')? + 1..];
    Some(after_quote[..after_quote.find('"')?].to_string())
}

/// Queries `releases.zigtools.org` for a ZLS build that matches `zig_version`.
///
/// Returns the ZLS version string and the download URL for the requested
/// platform. On failure (e.g. the version isn't indexed yet, or no network),
/// returns an `Err` so the caller can fall back to the latest stable release.
fn query_zls_for_zig_version(zig_version: &str, arch: &str, os: &str) -> Result<ZlsRelease> {
    // The `+` in Zig dev versions (e.g. `0.14.0-dev.123+abc`) must be
    // percent-encoded so the query parameter is parsed correctly.
    let encoded_version = zig_version.replace('+', "%2B");
    let url = format!(
        "{ZLS_RELEASE_API}?zig_version={encoded_version}&compatibility=only-runtime"
    );

    let response = HttpRequest::builder()
        .method(HttpMethod::Get)
        .url(&url)
        .redirect_policy(RedirectPolicy::FollowAll)
        .build()
        .map_err(|e| format!("failed to build ZLS API request: {e}"))?
        .fetch()
        .map_err(|e| format!("failed to query ZLS release API: {e}"))?;

    let body = String::from_utf8(response.body)
        .map_err(|e| format!("invalid UTF-8 in ZLS API response: {e}"))?;

    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse ZLS API response: {e}"))?;

    let version = json["version"]
        .as_str()
        .ok_or_else(|| {
            format!("ZLS API returned no version for Zig {zig_version} (response: {body})")
        })?
        .to_string();

    // Platforms are top-level keys in the response, e.g. "x86_64-macos".
    // The API returns .tar.xz URLs, but zed's download_file only supports GzipTar (.tar.gz).
    // Both formats are published, so we construct the .tar.gz URL from the version.
    let platform_key = format!("{arch}-{os}");
    // Verify the platform exists in the response before constructing the URL.
    json[&platform_key]
        .as_object()
        .ok_or_else(|| {
            format!("ZLS API response has no entry for platform '{platform_key}'")
        })?;
    let download_url = if os == "windows" {
        format!("https://builds.zigtools.org/zls-{platform_key}-{version}.zip")
    } else {
        format!("https://builds.zigtools.org/zls-{platform_key}-{version}.tar.gz")
    };

    Ok(ZlsRelease {
        version,
        download_url,
    })
}

impl ZigExtension {
    fn language_server_binary(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<ZlsBinary> {
        let mut args: Option<Vec<String>> = None;

        let (platform, arch) = zed::current_platform();
        let environment = match platform {
            zed::Os::Mac | zed::Os::Linux => Some(worktree.shell_env()),
            zed::Os::Windows => None,
        };

        if let Ok(lsp_settings) = LspSettings::for_worktree("zls", worktree) {
            if let Some(binary) = lsp_settings.binary {
                args = binary.arguments;
                if let Some(path) = binary.path {
                    return Ok(ZlsBinary {
                        path: path.clone(),
                        args,
                        environment,
                    });
                }
            }
        }

        if let Some(path) = worktree.which("zls") {
            return Ok(ZlsBinary {
                path,
                args,
                environment,
            });
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let arch_str: &str = match arch {
            zed::Architecture::Aarch64 => "aarch64",
            zed::Architecture::X86 => "x86",
            zed::Architecture::X8664 => "x86_64",
        };

        let os_str: &str = match platform {
            zed::Os::Mac => "macos",
            zed::Os::Linux => "linux",
            zed::Os::Windows => "windows",
        };

        // Prefer a ZLS build that matches the project's minimum_zig_version.
        // This is essential for master/nightly Zig where stable ZLS won't work.
        let zig_version: String = match worktree
            .read_text_file("build.zig.zon")
            .ok()
            .and_then(|zon| parse_minimum_zig_version(&zon))
        {
            Some(v) => v,
            None => {
                // No build.zig.zon or no minimum_zig_version field: fall back to
                // running `zig version`. Use worktree.which() so we respect the
                // user's PATH (important in WASM extension sandbox).
                let zig_path = worktree
                    .which("zig")
                    .ok_or_else(|| {
                        "`zig` not found on PATH and no `minimum_zig_version` in \
                         build.zig.zon. Please ensure `zig` is on your PATH or add \
                         `minimum_zig_version` to your build.zig.zon."
                            .to_string()
                    })?;
                let output = Command::new(&zig_path)
                    .arg("version")
                    .output()
                    .map_err(|e| format!("failed to run `{zig_path} version`: {e}"))?;
                if output.status != Some(0) {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!(
                        "`zig version` failed (exit {:?}): {stderr}",
                        output.status
                    ));
                }
                String::from_utf8(output.stdout)
                    .map_err(|_| "`zig version` output was not valid UTF-8".to_string())?
                    .trim()
                    .to_string()
            }
        };

        let release = query_zls_for_zig_version(&zig_version, arch_str, os_str)?;
        let (version, download_url) = (release.version, release.download_url);

        let version_dir = format!("zls-{}", version);
        let binary_path = match platform {
            zed::Os::Mac | zed::Os::Linux => format!("{version_dir}/zls"),
            zed::Os::Windows => format!("{version_dir}/zls.exe"),
        };

        // Return cached path if we already have this exact version on disk.
        if let Some((ref cached_version, ref cached_path)) = self.cached_binary {
            if cached_version == &version
                && fs::metadata(cached_path).is_ok_and(|s| s.is_file())
            {
                return Ok(ZlsBinary {
                    path: cached_path.clone(),
                    args,
                    environment,
                });
            }
        }

        if !fs::metadata(&binary_path).is_ok_and(|stat| stat.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );

            zed::download_file(
                &download_url,
                &version_dir,
                if download_url.ends_with(".zip") {
                    zed::DownloadedFileType::Zip
                } else {
                    zed::DownloadedFileType::GzipTar
                },
            )
            .map_err(|e| format!("failed to download ZLS: {e}"))?;

            zed::make_file_executable(&binary_path)?;

            let entries =
                fs::read_dir(".").map_err(|e| format!("failed to list working directory: {e}"))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
                if entry.file_name().to_str() != Some(&version_dir) {
                    fs::remove_dir_all(entry.path()).ok();
                }
            }
        }

        self.cached_binary = Some((version, binary_path.clone()));
        Ok(ZlsBinary {
            path: binary_path,
            args,
            environment,
        })
    }
}

impl zed::Extension for ZigExtension {
    fn new() -> Self {
        Self {
            cached_binary: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let zls_binary = self.language_server_binary(language_server_id, worktree)?;
        Ok(zed::Command {
            command: zls_binary.path,
            args: zls_binary.args.unwrap_or_default(),
            env: zls_binary.environment.unwrap_or_default(),
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let settings = LspSettings::for_worktree("zls", worktree)
            .ok()
            .and_then(|lsp_settings| lsp_settings.settings.clone())
            .unwrap_or_default();
        Ok(Some(settings))
    }

    fn dap_locator_create_scenario(
        &mut self,
        locator_name: String,
        build_task: zed::TaskTemplate,
        resolved_label: String,
        debug_adapter_name: String,
    ) -> Option<zed::DebugScenario> {
        if build_task.command != "zig" {
            return None;
        }

        let cwd = build_task.cwd.clone();
        let env = build_task.env.clone().into_iter().collect();

        let mut args_it = build_task.args.iter();
        let template = match args_it.next() {
            Some(arg) if arg == "build" => match args_it.next() {
                Some(arg) if arg == "run" => zed::BuildTaskTemplate {
                    label: "zig build".into(),
                    command: "zig".into(),
                    args: vec!["build".into()],
                    env,
                    cwd,
                },
                _ => return None,
            },
            Some(arg) if arg == "test" => {
                let test_exe_path = get_test_exe_path().unwrap();
                let mut args: Vec<String> = build_task
                    .args
                    .into_iter()
                    // TODO verify if this is required on non-Windows platforms
                    .map(|s| s.replace("\"", "'"))
                    .collect();
                args.push("--test-no-exec".into());
                args.push(format!("-femit-bin={test_exe_path}"));

                zed::BuildTaskTemplate {
                    label: "zig test --test-no-exec".into(),
                    command: "zig".into(),
                    args,
                    env,
                    cwd,
                }
            }
            Some(arg) if arg == "run" => zed::BuildTaskTemplate {
                label: "zig run".into(),
                command: "zig".into(),
                args: vec!["run".into()],
                env,
                cwd,
            },
            _ => return None,
        };

        let config = serde_json::Value::Null;
        let Ok(config) = serde_json::to_string(&config) else {
            return None;
        };

        Some(zed::DebugScenario {
            adapter: debug_adapter_name,
            label: resolved_label.clone(),
            config,
            tcp_connection: None,
            build: Some(zed::BuildTaskDefinition::Template(
                zed::BuildTaskDefinitionTemplatePayload {
                    template,
                    locator_name: Some(locator_name),
                },
            )),
        })
    }

    fn run_dap_locator(
        &mut self,
        _locator_name: String,
        build_task: zed::TaskTemplate,
    ) -> Result<zed::DebugRequest, String> {
        let mut args_it = build_task.args.iter();
        match args_it.next() {
            Some(arg) if arg == "build" => {
                // We only handle the default case where the binary name matches the project name.
                // This is valid for projects created with `zig init`.
                // In other cases, the user should provide a custom debug configuration.
                let exec = get_project_name(&build_task).ok_or("Failed to get project name")?;

                let request = zed::LaunchRequest {
                    program: format!("zig-out/bin/{exec}"),
                    cwd: build_task.cwd,
                    args: vec![],
                    envs: build_task.env.into_iter().collect(),
                };

                Ok(zed::DebugRequest::Launch(request))
            }
            Some(arg) if arg == "test" => {
                let program = build_task
                    .args
                    .iter()
                    .find_map(|arg| {
                        arg.strip_prefix("-femit-bin=").map(|arg| {
                            arg.split("=")
                                .nth(1)
                                .ok_or("Expected binary path in -femit-bin=")
                                .map(|path| path.trim_end_matches(".exe"))
                        })
                    })
                    .ok_or("Failed to extract binary path from command args")
                    .flatten()?
                    .to_string();
                let request = zed::LaunchRequest {
                    program,
                    cwd: build_task.cwd,
                    args: vec![],
                    envs: build_task.env.into_iter().collect(),
                };
                Ok(zed::DebugRequest::Launch(request))
            }
            _ => Err("Unsupported build task".into()),
        }
    }
}

fn get_project_name(task: &zed::TaskTemplate) -> Option<String> {
    task.cwd
        .as_ref()
        .and_then(|cwd| Some(Path::new(&cwd).file_name()?.to_string_lossy().into_owned()))
}

fn get_test_exe_path() -> Option<String> {
    let test_exe_dir = std::env::current_dir().ok()?;
    let mut name = format!("{}_{}", ZIG_TEST_EXE_BASENAME, uuid::Uuid::new_v4());
    if zed::current_platform().0 == zed::Os::Windows {
        name.push_str(".exe");
    }
    Some(test_exe_dir.join(name).to_string_lossy().into_owned())
}

zed::register_extension!(ZigExtension);
