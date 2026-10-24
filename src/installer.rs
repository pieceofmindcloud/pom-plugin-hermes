//! Self-contained, verified bootstrap. No Python, curl, git or Docker image is
//! required before the plugin starts. Hermes PM owns the Python/Node runtime.
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const REV: &str = "5894e750c3dba980d43b25aa29e3958149ba2c61";
const SOURCE_SHA: &str = "c6a3a8abdcebe82bc634761186b0fc51438bf5da06254ce0f486a3d6167ed1be";
const SOURCE_URL: &str = "https://codeload.github.com/NousResearch/hermes-agent/tar.gz/5894e750c3dba980d43b25aa29e3958149ba2c61";
const MAX_SOURCE: u64 = 512 * 1024 * 1024;
const MAX_UV: u64 = 100 * 1024 * 1024;
const PROXY_BASE: &str = "/api/ui/plugins/hermes/proxy/";
const WEB_MARKER: &str = ".pom-prefixed-web-v1";

#[derive(Clone, Copy)]
struct Artifact {
    url: &'static str,
    sha: &'static str,
    zipped: bool,
}
fn uv_artifact() -> Result<Artifact, String> {
    let target = (std::env::consts::OS, std::env::consts::ARCH);
    let (url, sha, zipped) = match target {
        ("linux", "x86_64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-x86_64-unknown-linux-gnu.tar.gz", "f68d291c75783122dad58ab77def0e13f3233f358731db5d6d7e8715603b928f", false),
        ("linux", "aarch64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-aarch64-unknown-linux-gnu.tar.gz", "4bb69cf95ed13f23713bb7bc09f241896ea0c75b7c557e615a20604b82f9c403", false),
        ("macos", "aarch64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-aarch64-apple-darwin.tar.gz", "743f85035a1996ecb350659e01cd9faea9709829be09b1b4b03c73eaf3c9cd73", false),
        ("macos", "x86_64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-x86_64-apple-darwin.tar.gz", "b0f52b0e3945b4107d89024ea38e625683011fbfc05a0d98f9fa2d6dcee28c66", false),
        ("windows", "x86_64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-x86_64-pc-windows-msvc.zip", "a38b8d1902b7cfbc76b423ba1bb456f623f42064cdc72628b6075b89afd1019a", true),
        ("windows", "aarch64") => ("https://github.com/astral-sh/uv/releases/download/0.13.5/uv-aarch64-pc-windows-msvc.zip", "09f47edf90726084a70f9426f6e3ca4f46e9bf7cbf23b1716ec64d60b055fae3", true),
        _ => return Err("Hermes PM does not support this operating system/architecture".into()),
    };
    Ok(Artifact { url, sha, zipped })
}

fn download(url: &str, target: &Path, sha: &str, max: u64) -> Result<(), String> {
    if target.is_file() && hash_file(target).is_ok_and(|hash| hash == sha) {
        return Ok(());
    }
    let part = target.with_extension("download");
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .redirects(5)
        .build()
        .get(url)
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(code, _) => {
                format!("Verified runtime download failed (HTTP {code})")
            }
            ureq::Error::Transport(transport) => format!(
                "Verified runtime download failed ({:?}); check network access",
                transport.kind()
            ),
        })?;
    let mut reader = response.into_reader();
    let mut out = fs::File::create(&part).map_err(|_| "Cannot write runtime download")?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut bytes = [0u8; 256 * 1024];
    let result = (|| -> Result<(), String> {
        loop {
            let read = reader
                .read(&mut bytes)
                .map_err(|_| "Runtime download interrupted")?;
            if read == 0 {
                break;
            }
            size += read as u64;
            if size > max {
                return Err("Runtime download exceeds size limit".into());
            }
            out.write_all(&bytes[..read])
                .map_err(|_| "Cannot write runtime download")?;
            hash.update(&bytes[..read]);
        }
        out.sync_all()
            .map_err(|_| "Cannot commit runtime download")?;
        if format!("{:x}", hash.finalize()) != sha {
            return Err("Runtime download checksum mismatch".into());
        }
        // Windows does not replace an existing destination with rename.
        if target.exists() {
            fs::remove_file(target).map_err(|_| "Cannot replace invalid runtime download")?;
        }
        fs::rename(&part, target).map_err(|_| "Cannot publish runtime download".to_owned())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}
fn hash_file(path: &Path) -> Result<String, io::Error> {
    let mut hash = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}
fn safe_file(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|p| matches!(p, Component::Normal(_)))
}
fn extract_source(archive: &Path, base: &Path) -> Result<PathBuf, String> {
    let source = base.join("source");
    if source.join(".verified").is_file() && source.join("pm/lock.json").is_file() {
        return Ok(source);
    }
    let stage = base.join("source.partial");
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(|_| "Cannot prepare Hermes source")?;
    let root = format!("hermes-agent-{REV}/");
    let file = fs::File::open(archive).map_err(|_| "Cannot open verified Hermes archive")?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    for entry in tar.entries().map_err(|_| "Invalid Hermes archive")? {
        let mut entry = entry.map_err(|_| "Invalid Hermes archive member")?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|_| "Invalid Hermes archive path")?
            .into_owned();
        if path == Path::new(root.trim_end_matches('/')) && entry.header().entry_type().is_dir() {
            continue;
        }
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| format!("Unexpected Hermes archive path: {}", path.display()))?;
        if !safe_file(relative) {
            return Err("Unsafe Hermes archive path".into());
        }
        let dest = stage.join(relative);
        if kind.is_dir() {
            fs::create_dir_all(dest).map_err(|_| "Cannot create Hermes directory")?;
        } else if kind.is_file() {
            fs::create_dir_all(dest.parent().ok_or("Invalid Hermes path")?)
                .map_err(|_| "Cannot create Hermes directory")?;
            let mut out = fs::File::create(&dest).map_err(|_| "Cannot write Hermes source")?;
            io::copy(&mut entry, &mut out).map_err(|_| "Cannot extract Hermes source")?;
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o755))
                    .map_err(|_| "Cannot set Hermes file permissions")?;
            }
        } else {
            return Err("Hermes archive contains unsupported file type".into());
        }
    }
    if !stage.join("pm/lock.json").is_file() || !stage.join("hermes_cli/main.py").is_file() {
        return Err("Verified Hermes source is incomplete".into());
    }
    fs::write(stage.join(".verified"), SOURCE_SHA).map_err(|_| "Cannot mark Hermes source")?;
    // Never overwrite a checkout containing a prior user install.
    if source.exists() {
        return Err(
            "Hermes source already exists without a verified install; move it aside to retry"
                .into(),
        );
    }
    fs::rename(&stage, &source).map_err(|_| "Cannot publish Hermes source")?;
    Ok(source)
}

fn extract_uv(archive: &Path, base: &Path, artifact: Artifact) -> Result<PathBuf, String> {
    let target = base.join(if cfg!(windows) { "uv.exe" } else { "uv" });
    let marker = base.join("uv.sha256");
    if target.is_file()
        && hash_file(&target).is_ok_and(|digest| {
            fs::read_to_string(&marker).ok().as_deref() == Some(digest.as_str())
        })
    {
        return Ok(target);
    }
    let stage = base.join("uv.partial");
    if artifact.zipped {
        let file = fs::File::open(archive).map_err(|_| "Cannot read uv archive")?;
        let mut zip = zip::ZipArchive::new(file).map_err(|_| "Invalid uv zip")?;
        let mut uv = zip.by_name("uv.exe").map_err(|_| "uv executable missing")?;
        let mut out = fs::File::create(&stage).map_err(|_| "Cannot write uv")?;
        io::copy(&mut uv, &mut out).map_err(|_| "Cannot extract uv")?;
    } else {
        let file = fs::File::open(archive).map_err(|_| "Cannot read uv archive")?;
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        let mut found = false;
        for entry in tar.entries().map_err(|_| "Invalid uv archive")? {
            let mut entry = entry.map_err(|_| "Invalid uv archive member")?;
            let path = entry.path().map_err(|_| "Invalid uv path")?;
            if path.file_name().is_some_and(|name| name == "uv")
                && entry.header().entry_type().is_file()
            {
                let mut out = fs::File::create(&stage).map_err(|_| "Cannot write uv")?;
                io::copy(&mut entry, &mut out).map_err(|_| "Cannot extract uv")?;
                found = true;
                break;
            }
        }
        if !found {
            return Err("uv executable missing".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))
                .map_err(|_| "Cannot execute uv")?;
        }
    }
    if target.exists() {
        fs::remove_file(&target).map_err(|_| "Cannot replace uv")?;
    }
    fs::rename(&stage, &target).map_err(|_| "Cannot publish uv")?;
    fs::write(
        marker,
        hash_file(&target).map_err(|_| "Cannot verify uv executable")?,
    )
    .map_err(|_| "Cannot mark uv extraction")?;
    Ok(target)
}

fn environment(cmd: &mut Command, base: &Path, home: &Path, source: &Path) {
    let boot = base.join("bootstrap");
    cmd.current_dir(source)
        .env("HERMES_HOME", home)
        .env("HERMES_RUNTIME_DIR", base.join("tools"))
        .env("UV_PYTHON_INSTALL_DIR", boot.join("pythons"))
        .env("UV_CACHE_DIR", boot.join("cache"))
        .env("UV_NO_CONFIG", "1")
        .env("UV_NO_PROJECT", "1")
        .env("PYTHONPATH", source)
        .env("CI", "1");
    #[cfg(unix)]
    {
        cmd.env("HOME", home);
    }
    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", home);
    }
}
fn seed_config(home: &Path, source: &Path) -> Result<(), String> {
    // Mirror the official installer's config stage without ever replacing
    // the model credentials configured by the plugin or a user's edits.
    for (sample, name) in [
        (".env.example", ".env"),
        ("cli-config.yaml.example", "config.yaml"),
    ] {
        let target = home.join(name);
        let output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target);
        let mut output = match output {
            Ok(output) => output,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("Cannot initialize Hermes settings".into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "Cannot protect Hermes settings")?;
        }
        io::copy(
            &mut fs::File::open(source.join(sample))
                .map_err(|_| "Hermes configuration template missing")?,
            &mut output,
        )
        .map_err(|_| "Cannot initialize Hermes settings")?;
    }
    Ok(())
}
fn find_python(base: &Path) -> Result<PathBuf, String> {
    let root = base.join("bootstrap/pythons");
    for entry in fs::read_dir(root)
        .map_err(|_| "Managed Python was not installed")?
        .flatten()
    {
        let dir = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("cpython-3.14.9-")
        {
            continue;
        }
        let path = if cfg!(windows) {
            dir.join("python.exe")
        } else {
            dir.join("bin/python3.14")
        };
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("Managed Python executable missing".into())
}

fn configure_web_base(source: &Path) -> Result<(), String> {
    let config_path = source.join("web/vite.config.ts");
    let config = fs::read_to_string(&config_path).map_err(|_| "Hermes web build config missing")?;
    let assignment = format!("  base: {PROXY_BASE:?},\n");
    if config.contains(&assignment) {
        return Ok(());
    }
    let anchor = "export default defineConfig({\n";
    if !config.contains(anchor) {
        return Err("Pinned Hermes web build config changed unexpectedly".into());
    }
    fs::write(
        config_path,
        config.replacen(anchor, &format!("{anchor}{assignment}"), 1),
    )
    .map_err(|_| "Cannot configure prefixed Hermes dashboard".to_owned())
}

fn web_is_prefixed(source: &Path) -> bool {
    fs::read_to_string(source.join("hermes_cli/web_dist/index.html"))
        .is_ok_and(|html| html.contains("/api/ui/plugins/hermes/proxy/assets/"))
}

fn ensure_web_base(
    base: &Path,
    home: &Path,
    python: &Path,
    source: &Path,
    run: &mut impl FnMut(Command) -> Result<(), String>,
) -> Result<(), String> {
    if source.join(WEB_MARKER).is_file() && web_is_prefixed(source) {
        return Ok(());
    }
    configure_web_base(source)?;
    let mut web = Command::new(python);
    web.args(["-c", "from pathlib import Path; from pm.environments import activate_dependencies; from hermes_cli.source_build import source_build_env, prepare_source_dependencies, build_source_web; root=Path.cwd(); activate_dependencies(root); env=source_build_env(explicit=True); prepare_source_dependencies(root, ('web',), env=env, explicit=True); build_source_web(root, env=env)"]);
    environment(&mut web, base, home, source);
    run(web)?;
    if !web_is_prefixed(source) {
        return Err(
            "Hermes dashboard assets do not use the POM proxy path; see install.log".into(),
        );
    }
    fs::write(source.join(WEB_MARKER), PROXY_BASE)
        .map_err(|_| "Cannot mark Hermes dashboard build".to_owned())
}

/// Register a named POM provider with Hermes's own configuration API. The
/// gateway key travels only in the child's environment, never argv or IPC.
pub fn register_pom(
    base: &Path,
    home: &Path,
    python: &Path,
    url: &str,
    key: &str,
    mut run: impl FnMut(Command) -> Result<(), String>,
) -> Result<(), String> {
    let script = base.join("pom_provider.py");
    fs::write(&script, include_str!("../pom_provider.py"))
        .map_err(|_| "Cannot prepare POM model integration")?;
    let source = base.join("source");
    let mut command = Command::new(python);
    command.arg(script).arg(url).env("POM_GATEWAY_KEY", key);
    environment(&mut command, base, home, &source);
    run(command)
}

/// `run` owns children so a plugin-host shutdown can stop installation.
pub fn ensure(
    base: &Path,
    home: &Path,
    mut run: impl FnMut(Command) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let source = base.join("source");
    let marker = base.join(".installed");
    if marker.is_file()
        && fs::read_to_string(&marker).ok().as_deref() == Some(SOURCE_SHA)
        && source.join("hermes_cli/web_dist/index.html").is_file()
    {
        let python = find_python(base)?;
        ensure_web_base(base, home, &python, &source, &mut run)?;
        return Ok(python);
    }
    let source_archive = base.join("source.tar.gz");
    download(SOURCE_URL, &source_archive, SOURCE_SHA, MAX_SOURCE)?;
    let source = extract_source(&source_archive, base)?;
    let uv_artifact = uv_artifact()?;
    let boot = base.join("bootstrap");
    fs::create_dir_all(&boot).map_err(|_| "Cannot prepare managed runtime")?;
    let uv_archive = boot.join(if uv_artifact.zipped {
        "uv.zip"
    } else {
        "uv.tar.gz"
    });
    download(uv_artifact.url, &uv_archive, uv_artifact.sha, MAX_UV)?;
    let uv = extract_uv(&uv_archive, &boot, uv_artifact)?;
    let mut python_install = Command::new(&uv);
    python_install.args(["python", "install", "--no-bin", "--no-registry", "3.14.9"]);
    environment(&mut python_install, base, home, &source);
    run(python_install)?;
    let python = find_python(base)?;
    let mut pm = Command::new(&python);
    pm.args([
        "-m",
        "pm.cli",
        "install",
        "--without",
        "agent-browser",
        "--without",
        "cua-driver",
    ]);
    environment(&mut pm, base, home, &source);
    run(pm)?;
    seed_config(home, &source)?;
    configure_web_base(&source)?;
    // The upstream installer's own completion tail builds the TUI and web
    // dashboard, syncs skills, migrates config and writes its source stamp.
    let mut completion = Command::new(&python);
    completion
        .args([
            "-I",
            "-B",
            "-X",
            "utf8",
            "hermes_cli/source_completion.py",
            "--source",
        ])
        .arg(&source);
    environment(&mut completion, base, home, &source);
    run(completion)?;
    if !web_is_prefixed(&source) {
        return Err(
            "Hermes dashboard assets do not use the POM proxy path; see install.log".into(),
        );
    }
    fs::write(source.join(WEB_MARKER), PROXY_BASE)
        .map_err(|_| "Cannot mark Hermes dashboard build".to_owned())?;
    fs::write(marker, SOURCE_SHA).map_err(|_| "Cannot mark Hermes installation")?;
    let _ = fs::remove_file(source_archive);
    Ok(python)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "downloads and installs the pinned Hermes runtime (several gigabytes)"]
    fn installs_without_system_python_git_or_curl() {
        let base = std::env::temp_dir().join("pom-hermes-standalone-e2e-no-tools");
        let home = base.join("user-data");
        fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let blocked = base.join("blocked-bin");
            fs::create_dir_all(&blocked).unwrap();
            for program in ["python3", "python", "git", "curl", "node", "npm"] {
                let path = blocked.join(program);
                fs::write(
                    &path,
                    "#!/bin/sh\necho forbidden-system-program >&2\nexit 42\n",
                )
                .unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let python = ensure(&base, &home, |mut command| {
            #[cfg(unix)]
            command.env(
                "PATH",
                format!("{}:/usr/bin:/bin", base.join("blocked-bin").display()),
            );
            let status = command.status().map_err(|e| e.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("installer exited: {status}"))
            }
        })
        .unwrap();
        assert!(python.is_file());
        assert!(base.join("source/hermes_cli/web_dist/index.html").is_file());
        let run_step = |mut command: Command| -> Result<(), String> {
            #[cfg(unix)]
            command.env(
                "PATH",
                format!("{}:/usr/bin:/bin", base.join("blocked-bin").display()),
            );
            let status = command.status().map_err(|e| e.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("installer exited: {status}"))
            }
        };
        // POM models: register the named provider with Hermes's own config
        // API. The gateway is unreachable in this sandbox, so the provider is
        // registered without a cached model list and the key stays in .env.
        register_pom(
            &base,
            &home,
            &python,
            "http://127.0.0.1:65432/v1",
            "sk-pom-e2e-fake",
            run_step,
        )
        .unwrap();
        let env_text = fs::read_to_string(home.join(".env")).unwrap();
        assert!(env_text.contains("POM_API_KEY=sk-pom-e2e-fake"));
        assert!(!env_text.contains("OPENAI_API_KEY=sk-pom-e2e-fake"));
        let config = fs::read_to_string(home.join("config.yaml")).unwrap();
        assert!(config.contains("pom:"));
        assert!(config.contains("base_url: http://127.0.0.1:65432/v1"));
        assert!(config.contains("key_env: POM_API_KEY"));
        let log = fs::File::create(base.join("bridge-test.log")).unwrap();
        let mut bridge = Command::new(&python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime.py"))
            .args([
                "serve",
                base.to_str().unwrap(),
                home.to_str().unwrap(),
                home.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        io::BufReader::new(bridge.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: serde_json::Value =
            serde_json::from_str(&line).expect("dashboard did not start; see bridge-test.log");
        let port = ready["port"].as_u64().unwrap();
        let token = ready["token"].as_str().unwrap();
        let agent = ureq::AgentBuilder::new()
            .timeout_read(Duration::from_secs(15))
            .build();
        for path in ["/", "/api/status"] {
            let url = format!("http://127.0.0.1:{port}{path}");
            let response = agent
                .get(&url)
                .set("x-pom-plugin-token", token)
                .call()
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.into_string().unwrap();
            assert!(!body.is_empty());
        }
        drop(bridge.stdin.take());
        assert!(bridge.wait().unwrap().success());
    }

    #[test]
    fn web_base_patch_is_precise_and_idempotent() {
        let base = std::env::temp_dir().join(format!("hermes-vite-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        fs::create_dir_all(source.join("web")).unwrap();
        fs::write(
            source.join("web/vite.config.ts"),
            "export default defineConfig({\n  plugins: [],\n})\n",
        )
        .unwrap();
        configure_web_base(&source).unwrap();
        let text = fs::read_to_string(source.join("web/vite.config.ts")).unwrap();
        assert_eq!(text.matches("base:").count(), 1);
        assert!(text.contains(&format!("base: {PROXY_BASE:?},")));
        configure_web_base(&source).unwrap();
        fs::write(
            source.join("web/vite.config.ts"),
            "export default defineConfig({ plugins: [] })\n",
        )
        .unwrap();
        assert!(configure_web_base(&source).is_err());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn defaults_do_not_replace_existing_settings() {
        let base = std::env::temp_dir().join(format!("hermes-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        let home = base.join("home");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(source.join(".env.example"), "DEFAULT=1").unwrap();
        fs::write(source.join("cli-config.yaml.example"), "default: true").unwrap();
        fs::write(home.join(".env"), "SECRET=existing").unwrap();
        seed_config(&home, &source).unwrap();
        seed_config(&home, &source).unwrap();
        assert_eq!(
            fs::read_to_string(home.join(".env")).unwrap(),
            "SECRET=existing"
        );
        assert_eq!(
            fs::read_to_string(home.join("config.yaml")).unwrap(),
            "default: true"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn pins_are_complete_and_paths_are_confined() {
        let artifact = uv_artifact().unwrap();
        assert_eq!(artifact.sha.len(), 64);
        assert_eq!(SOURCE_SHA.len(), 64);
        assert!(safe_file(Path::new("pm/lock.json")));
        assert!(!safe_file(Path::new("../private")));
        assert!(!safe_file(Path::new("/private")));
        assert!(!safe_file(Path::new("")));
    }
}
