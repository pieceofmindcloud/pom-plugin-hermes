//! Background install and supervised Hermes dashboard. IPC never waits for a download.
use crate::{context::Context, plugin_directory};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const SCRIPT: &str = include_str!("../runtime.py");

struct Running {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Clone)]
pub struct Status {
    pub phase: String,
    pub detail: String,
    port: Option<u16>,
    token: Option<String>,
}
impl Status {
    fn new(phase: &str, detail: &str) -> Self {
        Self {
            phase: phase.into(),
            detail: detail.into(),
            port: None,
            token: None,
        }
    }
    pub fn public(&self) -> Value {
        json!({"status": self.phase, "detail": self.detail})
    }
    pub fn upstream(&self) -> Value {
        if self.phase == "ready" {
            json!({"status":"ready", "port":self.port, "token":self.token})
        } else if self.phase == "error" {
            json!({"status":"error", "error":self.detail})
        } else {
            json!({"status":"starting"})
        }
    }
}
pub struct Supervisor {
    status: Mutex<Status>,
    process: Mutex<Option<Running>>,
    installer: Mutex<Option<Child>>,
    context: Mutex<Context>,
    busy: Mutex<bool>,
}
impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Status::new("waiting", "Install Hermes to begin")),
            process: Mutex::new(None),
            installer: Mutex::new(None),
            context: Mutex::new(Context::default()),
            busy: Mutex::new(false),
        })
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    fn set(&self, phase: &str, detail: &str) {
        *self.status.lock().unwrap() = Status::new(phase, detail);
    }
    pub fn configure(self: &Arc<Self>, context: Context) {
        *self.context.lock().unwrap() = context.clone();
        // After a restart, keep the operator's Hermes configuration and start
        // the existing installation. A new install is always opt-in.
        if let Ok(base) = plugin_directory() {
            if base.join(".installed").exists() {
                self.launch(None);
            }
        }
    }
    pub fn launch(self: &Arc<Self>, model: Option<String>) -> bool {
        let mut busy = self.busy.lock().unwrap();
        if *busy {
            return false;
        }
        // Reconfiguration must not stop a ready dashboard; no automatic restart
        // of an already running process merely to refresh a gateway credential.
        if model.is_none() && self.status().phase == "ready" {
            return true;
        }
        *busy = true;
        drop(busy);
        let this = Arc::clone(self);
        thread::spawn(move || {
            if let Err(error) = this.start(model) {
                this.stop();
                // Do not include subprocess output, environment or credentials.
                this.set("error", &error);
            }
            *this.busy.lock().unwrap() = false;
        });
        true
    }
    fn start(&self, model: Option<String>) -> Result<(), String> {
        let base = plugin_directory()?;
        fs::create_dir_all(&base).map_err(|_| "Cannot create Hermes runtime directory")?;
        private_directory(&base)?;
        let script = base.join("runtime.py");
        fs::write(&script, SCRIPT).map_err(|_| "Cannot prepare Hermes installer")?;
        self.stop();
        let home = self
            .context
            .lock()
            .unwrap()
            .workspace_state_dir()
            .unwrap_or_else(|| base.join("data"));
        fs::create_dir_all(&home).map_err(|_| "Cannot create Hermes data directory")?;
        private_directory(&home)?;
        // The upstream installer seeds config.yaml/.env when absent. Put the
        // optional POM provider in place first so it cannot be overwritten.
        if let Some(model) = model {
            if !home.join("config.yaml").is_file() {
                configure_model(&home, &model, &self.context.lock().unwrap())?;
            }
        }
        self.set(
            "installing",
            "Downloading and building pinned Hermes Agent (see install.log)",
        );
        let log =
            fs::File::create(base.join("install.log")).map_err(|_| "Cannot open install log")?;
        let errors = log.try_clone().map_err(|_| "Cannot open install log")?;
        let mut installer = Command::new("python3");
        installer
            .args([
                script.as_os_str(),
                "install".as_ref(),
                base.as_os_str(),
                home.as_os_str(),
            ])
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(errors);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            installer.process_group(0);
            #[cfg(target_os = "linux")]
            unsafe {
                installer.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        *self.installer.lock().unwrap() = Some(
            installer
                .spawn()
                .map_err(|_| "Python 3, bash, git and network access are required")?,
        );
        loop {
            let outcome = {
                let mut installer = self.installer.lock().unwrap();
                match installer.as_mut() {
                    None => return Err("Hermes installation interrupted".into()),
                    Some(child) => match child.try_wait() {
                        Ok(Some(exit)) => {
                            *installer = None;
                            Some(exit)
                        }
                        Ok(None) => None,
                        Err(_) => return Err("Cannot monitor Hermes installation".into()),
                    },
                }
            };
            if let Some(exit) = outcome {
                if !exit.success() {
                    return Err("Hermes installation failed; see install.log".into());
                }
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        self.set("starting", "Starting Hermes dashboard");
        let log =
            fs::File::create(base.join("bridge.log")).map_err(|_| "Cannot open bridge log")?;
        let mut cmd = Command::new("python3");
        cmd.arg(script)
            .arg("serve")
            .arg(&base)
            .arg(&home)
            .current_dir(&base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log);
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|_| "Cannot start Hermes dashboard bridge")?;
        let stdin = child.stdin.take().ok_or("Bridge stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("Bridge stdout unavailable")?;
        *self.process.lock().unwrap() = Some(Running { child, stdin });
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(|_| "Bridge failed to start; see bridge.log")?;
        let reply: Value = serde_json::from_str(&line)
            .map_err(|_| "Dashboard failed to start; see dashboard.log")?;
        let port = reply["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .ok_or("Invalid bridge port")?;
        let token = reply["token"]
            .as_str()
            .filter(|s| s.len() >= 16)
            .ok_or("Missing bridge token")?;
        *self.status.lock().unwrap() = Status {
            phase: "ready".into(),
            detail: "Hermes dashboard is running".into(),
            port: Some(port),
            token: Some(token.into()),
        };
        loop {
            thread::sleep(Duration::from_secs(2));
            let mut process = self.process.lock().unwrap();
            let Some(running) = process.as_mut() else {
                break;
            };
            if let Ok(Some(_)) = running.child.try_wait() {
                *process = None;
                drop(process);
                self.set("error", "Hermes dashboard stopped; see dashboard.log");
                break;
            }
        }
        Ok(())
    }
    pub(crate) fn stop(&self) {
        if let Some(mut child) = self.installer.lock().unwrap().take() {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGTERM);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut running) = self.process.lock().unwrap().take() {
            drop(running.stdin);
            for _ in 0..30 {
                if matches!(running.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot secure Hermes directory")?;
    }
    Ok(())
}

fn configure_model(home: &Path, model: &str, context: &Context) -> Result<(), String> {
    let gateway = context.gateway.as_ref().ok_or("POM gateway unavailable")?;
    let model = model.trim();
    if model.is_empty()
        || model.len() > 160
        || model
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || "/._:-".contains(c)))
    {
        return Err("Choose a valid deployed POM model name".into());
    }
    // Never overwrite a Hermes config managed in its dashboard. Initial
    // connection is created once; Hermes itself owns further changes.
    if home.join("config.yaml").exists() {
        return Err("Hermes is already configured; edit its model in the dashboard".into());
    }
    let url = gateway.openai_base_url.trim_end_matches('/');
    if !url.starts_with("http://127.0.0.1:") && !url.starts_with("http://localhost:") {
        return Err("POM gateway must be loopback".into());
    }
    if home.join(".env").exists() {
        return Err("Hermes credentials already exist; configure in its dashboard".into());
    }
    if gateway.api_key.contains(['\n', '\r']) {
        return Err("Invalid POM API key".into());
    }
    fs::write(
        home.join("config.yaml"),
        format!(
            "model:\n  provider: custom\n  default: {}\n  base_url: {}\n",
            json!(model),
            json!(url)
        ),
    )
    .map_err(|_| "Cannot write Hermes configuration")?;
    let env_path = home.join(".env");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(env_path)
        .map_err(|_| "Cannot save Hermes credentials")?;
    writeln!(file, "OPENAI_API_KEY={}", gateway.api_key)
        .map_err(|_| "Cannot save Hermes credentials")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_never_leaks_token() {
        let status = Status {
            phase: "ready".into(),
            detail: "ok".into(),
            port: Some(42),
            token: Some("secret-secret-secret".into()),
        };
        assert!(!status.public().to_string().contains("secret"));
        assert_eq!(status.upstream()["token"], "secret-secret-secret");
    }

    #[test]
    fn first_model_setup_is_private_and_never_overwrites_hermes_settings() {
        let home = std::env::temp_dir().join(format!("hermes-setup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let context = Context::from_configure(&json!({"gateway": {
            "openai_base_url":"http://127.0.0.1:8080/v1", "api_key":"sk-private"
        }}))
        .unwrap();
        assert!(configure_model(&home, "invalid model", &context).is_err());
        assert!(!home.join("config.yaml").exists());
        configure_model(&home, "org/model", &context).unwrap();
        let config = fs::read_to_string(home.join("config.yaml")).unwrap();
        assert!(config.contains("default: \"org/model\""));
        assert!(!config.contains("sk-private"));
        assert_eq!(
            fs::read_to_string(home.join(".env")).unwrap(),
            "OPENAI_API_KEY=sk-private\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(home.join(".env"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(configure_model(&home, "another", &context).is_err());
        fs::remove_dir_all(home).unwrap();
    }
}
