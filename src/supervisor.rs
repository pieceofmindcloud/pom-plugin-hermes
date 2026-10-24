//! Background install and supervised Hermes dashboard. IPC never waits for a download.
use crate::{
    context::Context,
    installer::{self, InstallMode, Step},
    plugin_directory,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
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
    /// While installing: the current step, when it started and, for a
    /// download, the fraction done.
    step: Option<(Step, Instant, Option<f64>)>,
    /// While installing: `install` or `update`.
    mode: Option<&'static str>,
}
impl Status {
    fn new(phase: &str, detail: &str) -> Self {
        Self {
            phase: phase.into(),
            detail: detail.into(),
            port: None,
            token: None,
            step: None,
            mode: None,
        }
    }
    pub fn public(&self) -> Value {
        let mut public = json!({"status": self.phase, "detail": self.detail});
        if self.phase == "installing" {
            if let Some((step, since, measured)) = self.step {
                public["step"] = json!(step.code());
                public["progress"] = json!(progress_at(step, since.elapsed(), measured));
            }
            if let Some(mode) = self.mode {
                public["mode"] = json!(mode);
            }
        }
        public
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
/// Each step's share of the bar, `(start, end)`, and for an unmeasured step
/// the time constant of its creep.
fn weights(step: Step) -> (f64, f64, f64) {
    match step {
        Step::Source => (0.0, 5.0, 0.0),
        Step::Uv => (5.0, 8.0, 0.0),
        Step::Python => (8.0, 20.0, 30.0),
        Step::Packages => (20.0, 75.0, 180.0),
        Step::Dashboard => (75.0, 95.0, 90.0),
        Step::Connect => (95.0, 100.0, 5.0),
    }
}

/// The bar while `step` runs: a download follows its bytes, any other step
/// creeps toward its end with time. Neither reaches the end: that value is the
/// next step's start, reported only when the step really finished.
pub fn progress_at(step: Step, elapsed: Duration, measured: Option<f64>) -> u8 {
    let (start, end, tau) = weights(step);
    let fraction = match measured {
        Some(done) => done.clamp(0.0, 1.0),
        None if tau > 0.0 => 1.0 - (-elapsed.as_secs_f64() / tau).exp(),
        None => 0.0,
    };
    (start + (end - start) * 0.95 * fraction).floor() as u8
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
        // the existing installation. A new install starts when someone opens
        // the Hermes screen: opening it is the opt-in.
        if let Ok(base) = plugin_directory() {
            if base.join(".installed").exists() {
                self.launch(None);
            }
        }
    }
    /// `None` while another launch runs; `Some(false)` when the dashboard is
    /// already up and nothing has to start; `Some(true)` when the caller starts
    /// it, the phase already moved off any previous error: `rpc.install` answers
    /// before the thread runs, and the screen's next poll must not read the old
    /// error as the new attempt's.
    fn begin_launch(&self, reconfigure: bool) -> Option<bool> {
        let mut busy = self.busy.lock().unwrap();
        if *busy {
            return None;
        }
        // Reconfiguration must not stop a ready dashboard; no automatic restart
        // of an already running process merely to refresh a gateway credential.
        if !reconfigure && self.status().phase == "ready" {
            return Some(false);
        }
        *busy = true;
        self.set("starting", "Starting Hermes dashboard");
        Some(true)
    }
    pub fn launch(self: &Arc<Self>, model: Option<String>) -> bool {
        match self.begin_launch(model.is_some()) {
            None => return false,
            Some(false) => return true,
            Some(true) => {}
        }
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
        fs::write(&script, SCRIPT).map_err(|_| "Cannot prepare Hermes bridge")?;
        self.stop();
        let (home, workspace) = {
            let context = self.context.lock().unwrap();
            let home = context
                .workspace_state_dir()
                .unwrap_or_else(|| base.join("data"));
            // The agent works in POM's workspace, not in Hermes's own source.
            let workspace = context
                .workspace_root
                .clone()
                .unwrap_or_else(|| home.clone());
            (home, workspace)
        };
        fs::create_dir_all(&home).map_err(|_| "Cannot create Hermes data directory")?;
        private_directory(&home)?;
        // The upstream installer seeds config.yaml/.env when absent. Put the
        // optional POM provider in place first so it cannot be overwritten.
        if let Some(model) = model {
            if !home.join("config.yaml").is_file() {
                configure_model(&home, &model, &self.context.lock().unwrap())?;
            }
        }
        // Only a fresh install or an update shows the installer; starting an
        // installed Hermes still checks it, under the generic loader.
        let mode = match installer::install_mode(&base) {
            InstallMode::Fresh => Some("install"),
            InstallMode::Update => Some("update"),
            InstallMode::Installed => None,
        };
        match mode {
            Some(_) => self.set(
                "installing",
                "Installing pinned Hermes Agent (see install.log)",
            ),
            None => self.set("starting", "Starting Hermes dashboard"),
        }
        let report = |step: Step, measured: Option<f64>| {
            let Some(mode) = mode else { return };
            let mut status = self.status.lock().unwrap();
            let since = match status.step {
                Some((current, since, _)) if current == step => since,
                _ => Instant::now(),
            };
            status.step = Some((step, since, measured));
            status.mode = Some(mode);
        };
        let python = installer::ensure(
            &base,
            &home,
            |command| self.run_install_step(command, &base),
            &report,
        )?;
        if let Some(gateway) = self.context.lock().unwrap().gateway.clone() {
            report(Step::Connect, None);
            installer::register_pom(
                &base,
                &home,
                &python,
                &gateway.openai_base_url,
                &gateway.api_key,
                |command| self.run_install_step(command, &base),
            )?;
        }
        self.set("starting", "Starting Hermes dashboard");
        let log =
            fs::File::create(base.join("bridge.log")).map_err(|_| "Cannot open bridge log")?;
        let mut cmd = Command::new(python);
        cmd.arg(script)
            .arg("serve")
            .arg(&base)
            .arg(&home)
            .arg(&workspace)
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
            step: None,
            mode: None,
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
    fn run_install_step(&self, mut command: Command, base: &Path) -> Result<(), String> {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("install.log"))
            .map_err(|_| "Cannot open Hermes install log")?;
        let errors = log
            .try_clone()
            .map_err(|_| "Cannot open Hermes install log")?;
        command.stdin(Stdio::null()).stdout(log).stderr(errors);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            #[cfg(target_os = "linux")]
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        *self.installer.lock().unwrap() = Some(
            command
                .spawn()
                .map_err(|_| "Cannot start Hermes managed runtime; see install.log")?,
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
                return if exit.success() {
                    Ok(())
                } else {
                    Err("Hermes installation failed; see install.log".into())
                };
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
    pub(crate) fn stop(&self) {
        if let Some(mut child) = self.installer.lock().unwrap().take() {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGTERM);
            }
            #[cfg(windows)]
            {
                // PM and npm spawn grandchildren. taskkill is a Windows OS
                // command, not an install-time prerequisite for Hermes.
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &child.id().to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut running) = self.process.lock().unwrap().take() {
            // Closing stdin makes the bridge end the dashboard and everything
            // Hermes detached (gateway, TUIs); give its 2 s grace time to finish.
            drop(running.stdin);
            for _ in 0..50 {
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
    #[cfg(not(unix))]
    let _ = path;
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
    use crate::installer::Step;
    use std::time::Duration;
    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn an_unmeasured_step_starts_at_its_start_and_never_reaches_its_end() {
        assert_eq!(progress_at(Step::Packages, Duration::ZERO, None), 20);
        assert!(progress_at(Step::Packages, SECOND * 10_000, None) < 75);
        let mut last = 0;
        for seconds in 0..600 {
            let now = progress_at(Step::Packages, SECOND * seconds, None);
            assert!(now >= last, "went back at {seconds}s");
            last = now;
        }
        assert!(last > 60, "still crawling at 10 minutes: {last}");
    }

    #[test]
    fn a_measured_step_follows_its_fraction() {
        assert_eq!(progress_at(Step::Source, Duration::ZERO, Some(0.5)), 2);
        assert_eq!(progress_at(Step::Uv, Duration::ZERO, Some(1.0)), 7);
        assert_eq!(progress_at(Step::Connect, SECOND * 600, None), 99);
    }

    #[test]
    fn an_installing_status_carries_step_progress_and_mode() {
        let mut status = Status::new("installing", "Installing");
        status.mode = Some("update");
        status.step = Some((Step::Python, std::time::Instant::now(), None));
        let public = status.public();
        assert_eq!(public["step"], "python");
        assert_eq!(public["progress"], 8);
        assert_eq!(public["mode"], "update");
        let ready = Status::new("ready", "ok").public();
        assert!(
            ready.get("step").is_none()
                && ready.get("progress").is_none()
                && ready.get("mode").is_none()
        );
    }

    #[test]
    fn an_accepted_launch_leaves_the_error_at_once() {
        // `rpc.install` returns before the install thread runs; a poll in
        // between must not read the previous error, or a retry that fails
        // again at once is never seen as a new error.
        let supervisor = Supervisor::new();
        supervisor.set("error", "Runtime download interrupted");
        assert_eq!(supervisor.begin_launch(false), Some(true));
        assert_eq!(supervisor.status().phase, "starting");
        assert_eq!(
            supervisor.begin_launch(false),
            None,
            "a second launch while busy"
        );
    }

    #[test]
    fn status_never_leaks_token() {
        let status = Status {
            phase: "ready".into(),
            detail: "ok".into(),
            port: Some(42),
            token: Some("secret-secret-secret".into()),
            step: None,
            mode: None,
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
