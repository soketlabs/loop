//! One place that turns sandbox settings or `/sandbox local` flags into a running sandbox.
//!
//! Both `/sandbox local` and the Session factory go through [`SandboxBuilder`]. The
//! builder selects a Workspace driver by data, not by `#[cfg]`: the sandbox kind
//! (`sandbox.mode`, [`LOCAL_SANDBOX_KIND`]) and the platform go to
//! [`SandboxRegistry::select`], which returns the first registered driver that
//! serves the kind and supports the platform. The built-in drivers are `podman`
//! (Linux, [`KrunSandboxFactory`](loop_agent::harness::KrunSandboxFactory)) and
//! `seatbelt` (macOS). A driver from another crate registers itself with
//! [`register_workspace_driver`](loop_agent::harness::sandbox::register_workspace_driver);
//! [`platform_registry`] adds it after the built-in drivers.
//!
//! Isolation and runtime are optional. A value the user set (a flag, or a key in
//! `settings.json`) is explicit; a missing value inherits the platform default. An
//! explicit Linux-only value on macOS is an error; an inherited default never is.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loop_agent::harness::sandbox::{
    builtin_workspace_drivers, HostOs, NetworkPolicy, PathNamespace, Workspace, WorkspaceDriver,
};
use loop_agent::harness::{KrunIsolation, LocalSandboxRuntime, SandboxConfig, SandboxRegistry};
use serde_json::json;

use crate::config::settings::SandboxSettings;

/// Registry kind for the platform's local sandbox.
pub const LOCAL_SANDBOX_KIND: &str = "local";

/// Operating system family, which decides sandbox defaults and what is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPlatform {
    /// Linux: Podman containers.
    Linux,
    /// macOS: Seatbelt partial isolation.
    MacOs,
    /// Anything else: no local sandbox.
    Other,
}

impl SandboxPlatform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        match HostOs::current() {
            HostOs::Linux => Self::Linux,
            HostOs::MacOs => Self::MacOs,
            HostOs::Other => Self::Other,
        }
    }

    /// The platform as driver selection data.
    pub fn os(self) -> HostOs {
        match self {
            Self::Linux => HostOs::Linux,
            Self::MacOs => HostOs::MacOs,
            Self::Other => HostOs::Other,
        }
    }

    /// Isolation used when the user did not choose one.
    pub fn default_isolation(self) -> KrunIsolation {
        match self {
            Self::MacOs => KrunIsolation::Partial,
            Self::Linux | Self::Other => KrunIsolation::Full,
        }
    }

    /// Container runtime used when the user did not choose one; `None` where the
    /// sandbox does not use containers.
    pub fn default_runtime(self) -> Option<LocalSandboxRuntime> {
        match self {
            Self::Linux => Some(LocalSandboxRuntime::Runc),
            Self::MacOs | Self::Other => None,
        }
    }

    fn name(self) -> &'static str {
        self.os().name()
    }
}

/// Isolation and runtime chosen with `/sandbox local` flags. `None` means not given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalSandboxFlags {
    /// `--full` or `--partial`.
    pub isolation: Option<KrunIsolation>,
    /// `--crun`, `--runc`, `--runsc` or `--krun`.
    pub runtime: Option<LocalSandboxRuntime>,
}

/// Parse flags after `/sandbox local` (`--full|--partial`, `--crun|--runc|--runsc|--krun`).
///
/// Positional `full`/`partial`/`runc`/… are accepted too. A flag left out stays `None`,
/// so the platform default applies.
pub fn parse_local_sandbox_flags(args: &[&str]) -> Result<LocalSandboxFlags, String> {
    let mut flags = LocalSandboxFlags::default();
    for tok in args {
        let raw = tok.trim();
        if raw.is_empty() {
            continue;
        }
        let key = raw.trim_start_matches('-').to_ascii_lowercase();
        match key.as_str() {
            "full" | "partial" => {
                if flags.isolation.is_some() {
                    return Err("specify only one of --full / --partial".into());
                }
                flags.isolation = KrunIsolation::parse(&key);
            }
            "crun" | "runc" | "runsc" | "gvisor" | "krun" => {
                if flags.runtime.is_some() {
                    return Err("specify only one runtime (--crun|--runc|--runsc|--krun)".into());
                }
                flags.runtime = LocalSandboxRuntime::parse(&key);
            }
            other => {
                return Err(format!(
                    "unknown sandbox option '{other}' (use --full|--partial and --crun|--runc|--runsc|--krun)"
                ));
            }
        }
    }
    Ok(flags)
}

/// Which path policy the Session uses. Path rules check paths through the
/// Session's Workspace env (task C2b), but some of their checks still use
/// host forms, so a guest-namespace driver is refused with them for now.
/// The tool lock cannot work with one at all: its root is a Runtime host
/// folder, admitted on the host, and its second layer confines host reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PathPolicyUse {
    /// Not stated: treated as the tool lock.
    #[default]
    Unknown,
    /// No path rules and no tool lock.
    None,
    /// Path rules, and no tool lock. Refused with a guest driver until the
    /// guest leftovers of task C2b are done.
    Rules,
    /// The tool lock applies.
    ToolLock,
}

/// What the user asked for, before platform defaults are applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxRequest {
    /// Sandbox kind (`sandbox.mode`); `None` means [`LOCAL_SANDBOX_KIND`].
    pub kind: Option<String>,
    /// Which path policy applies to the Session. `Unknown` counts as the
    /// tool lock.
    pub path_policy: PathPolicyUse,
    /// Explicit isolation, if any.
    pub isolation: Option<KrunIsolation>,
    /// Explicit container runtime, if any.
    pub runtime: Option<LocalSandboxRuntime>,
    /// Allow network access from sandboxed commands.
    pub network: bool,
    /// Extra writable paths (`~` expanded; relative paths are under the workspace).
    pub allow_write: Vec<PathBuf>,
    /// Deny reads under the home directory except the readable paths.
    pub deny_home_reads: bool,
    /// Paths under the home directory that stay readable (`~` expanded).
    pub allow_read: Vec<PathBuf>,
}

impl SandboxRequest {
    /// Read the request from settings. An unknown isolation or runtime is an error.
    pub fn from_settings(settings: &SandboxSettings) -> Result<Self, SandboxBuildError> {
        let isolation = settings
            .isolation
            .as_deref()
            .map(|value| {
                KrunIsolation::parse(value).ok_or_else(|| SandboxBuildError::InvalidSetting {
                    key: "sandbox.isolation",
                    value: value.into(),
                    expected: "full or partial",
                })
            })
            .transpose()?;
        let runtime = settings
            .runtime
            .as_deref()
            .map(|value| {
                LocalSandboxRuntime::parse(value).ok_or_else(|| SandboxBuildError::InvalidSetting {
                    key: "sandbox.runtime",
                    value: value.into(),
                    expected: "crun, runc, runsc or krun",
                })
            })
            .transpose()?;
        // Older builds saved this exact pair into every settings file, whether or
        // not the user chose it. Read it as unset so it inherits the platform
        // default (the same values on Linux; no error on macOS).
        let (isolation, runtime) = if isolation == Some(KrunIsolation::Full)
            && runtime == Some(LocalSandboxRuntime::Runc)
        {
            (None, None)
        } else {
            (isolation, runtime)
        };
        Ok(Self {
            kind: (settings.mode != "off").then(|| settings.mode.clone()),
            path_policy: PathPolicyUse::Unknown,
            isolation,
            runtime,
            network: settings.network(),
            allow_write: settings
                .allow_write
                .iter()
                .map(|p| expand_home(p))
                .collect(),
            deny_home_reads: settings.deny_home_reads(),
            allow_read: settings.allow_read.iter().map(|p| expand_home(p)).collect(),
        })
    }

    /// Request for `/sandbox local`: isolation and runtime come from the flags only
    /// (a flag left out falls back to the platform default, not to the saved
    /// setting), while network and writable paths come from settings. Saved
    /// isolation and runtime are not read, so a bad saved value never blocks the
    /// command that replaces it.
    pub fn for_flags(settings: &SandboxSettings, flags: LocalSandboxFlags) -> Self {
        Self {
            kind: None,
            path_policy: PathPolicyUse::Unknown,
            isolation: flags.isolation,
            runtime: flags.runtime,
            network: settings.network(),
            allow_write: settings
                .allow_write
                .iter()
                .map(|p| expand_home(p))
                .collect(),
            deny_home_reads: settings.deny_home_reads(),
            allow_read: settings.allow_read.iter().map(|p| expand_home(p)).collect(),
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// A request with platform defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    /// Isolation to use.
    pub isolation: KrunIsolation,
    /// Container runtime, where the platform sandbox uses containers.
    pub runtime: Option<LocalSandboxRuntime>,
    /// Allow network access.
    pub network: bool,
    /// Extra writable paths.
    pub allow_write: Vec<PathBuf>,
    /// Deny reads under the home directory except the readable paths.
    pub deny_home_reads: bool,
    /// Paths under the home directory that stay readable.
    pub allow_read: Vec<PathBuf>,
}

impl SandboxPlan {
    /// Flags as shown to people, e.g. `--full --runc` or `--partial`.
    pub fn label(&self) -> String {
        match self.runtime {
            Some(runtime) => format!("--{} --{}", self.isolation.as_str(), runtime.as_str()),
            None => format!("--{}", self.isolation.as_str()),
        }
    }

    /// Factory config for `workdir`. Backends read the keys they support:
    /// the Podman sandbox reads `isolation` and `runtime`, and ignores
    /// `network` and `allowWrite` for now.
    pub fn config(&self, workdir: &Path) -> SandboxConfig {
        self.config_for_agent(workdir, &crate::config::paths::get_agent_dir())
    }

    fn config_for_agent(&self, workdir: &Path, agent_dir: &Path) -> SandboxConfig {
        let allow_write: Vec<String> = self
            .allow_write
            .iter()
            .map(|p| workdir.join(p).display().to_string())
            .collect();
        let mut options = json!({
            "isolation": self.isolation.as_str(),
            "network": self.network,
            "allowWrite": allow_write,
            "denyHomeReads": self.deny_home_reads,
            "allowRead": self
                .allow_read
                .iter()
                .map(|p| workdir.join(p).display().to_string())
                .collect::<Vec<_>>(),
            "homeDir": dirs::home_dir(),
            "agentDir": agent_dir,
        });
        // Cover Runtime's short-path socket fallback without creating any
        // Runtime directories while merely preparing a sandbox config.
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no arguments or preconditions.
            let uid = unsafe { libc::geteuid() };
            options["runtimePaths"] = json!([format!("/tmp/loop-{uid}")]);
        }
        if let Some(runtime) = self.runtime {
            options["runtime"] = json!(runtime.as_str());
        }
        SandboxConfig {
            workdir: workdir.to_path_buf(),
            options,
            labels: Default::default(),
        }
    }
}

/// Why a sandbox could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBuildError {
    /// A settings value is not recognised.
    InvalidSetting {
        /// Settings key.
        key: &'static str,
        /// The value found.
        value: String,
        /// The accepted values.
        expected: &'static str,
    },
    /// The user chose a Linux-only container runtime on another platform.
    LinuxOnlyRuntime {
        /// Platform in use.
        platform: SandboxPlatform,
        /// The runtime chosen.
        runtime: LocalSandboxRuntime,
    },
    /// The user chose full isolation where it is not available.
    FullIsolationUnavailable {
        /// Platform in use.
        platform: SandboxPlatform,
    },
    /// No sandbox exists for this platform yet.
    Unavailable {
        /// Platform in use.
        platform: SandboxPlatform,
    },
    /// The selected driver does not meet a setting or a rule. Selection
    /// refuses instead of running with less protection.
    Unmet {
        /// Driver id.
        driver: String,
        /// The setting or rule.
        setting: &'static str,
        /// Why not.
        reason: &'static str,
    },
    /// The sandbox could not be created or started.
    Start(String),
}

impl fmt::Display for SandboxBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSetting {
                key,
                value,
                expected,
            } => write!(f, "{key} is \"{value}\" in settings; use {expected}"),
            Self::LinuxOnlyRuntime { platform, runtime } => write!(
                f,
                "--{rt} is a Linux container runtime and is not available on {os}. \
                 Leave out --{rt}, and remove sandbox.runtime from settings.json if it is set there.",
                rt = runtime.as_str(),
                os = platform.name(),
            ),
            Self::FullIsolationUnavailable { platform } => write!(
                f,
                "--full isolation is not available on {os} yet. Use --partial or leave it out, \
                 and remove sandbox.isolation from settings.json if it is set to \"full\" there.",
                os = platform.name(),
            ),
            Self::Unavailable { platform } => {
                write!(
                    f,
                    "the local sandbox is not available on {} yet; tools run on the host (/sandbox off).",
                    platform.name()
                )?;
                if *platform == SandboxPlatform::MacOs {
                    f.write_str(
                        " Use `--permission-mode read-only` or `--permission-mode ask` to control tool permissions.",
                    )?;
                }
                Ok(())
            }
            Self::Unmet {
                driver,
                setting,
                reason,
            } => write!(
                f,
                "the `{driver}` workspace driver does not meet {setting}: {reason}"
            ),
            Self::Start(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SandboxBuildError {}

/// Refuse a driver that would run with less protection than the request
/// asks for. Capabilities only add restrictions or refuse; they never relax
/// a setting.
///
/// - A driver whose paths are guest paths is refused with the tool lock (or
///   when the caller does not say): the lock's root and its confined reads
///   are Runtime host paths. It is also refused with path rules until the
///   guest leftovers of task C2b are done (`~`, subagent rule bases, the
///   search filter).
/// - For kinds other than `local` (hosted and remote use), every
///   restriction the user set must be enforced. `local` keeps today's
///   behaviour (for example Linux does not enforce `sandbox.network` yet).
fn check_driver(
    driver: &dyn WorkspaceDriver,
    kind: &str,
    request: &SandboxRequest,
    plan: &SandboxPlan,
) -> Result<(), SandboxBuildError> {
    let caps = driver.capabilities();
    let unmet = |setting, reason| SandboxBuildError::Unmet {
        driver: driver.id().to_owned(),
        setting,
        reason,
    };
    if caps.path_namespace == PathNamespace::Guest
        && matches!(
            request.path_policy,
            PathPolicyUse::Unknown | PathPolicyUse::ToolLock
        )
    {
        return Err(unmet(
            "the tool lock",
            "its root and its confined reads are Runtime host paths, and this driver's paths are not host paths",
        ));
    }
    // The gate checks path rules through the env (C2b), but `~` rules,
    // subagent rule bases and the search filter still use host forms
    // (docs/design/workspace-driver.md section 6). Refused until those are
    // done.
    if caps.path_namespace == PathNamespace::Guest && request.path_policy == PathPolicyUse::Rules {
        return Err(unmet(
            "path permission rules",
            "some of their checks still use Runtime host paths, and this driver's paths are not host paths",
        ));
    }
    if kind == LOCAL_SANDBOX_KIND {
        return Ok(());
    }
    if !plan.network
        && !matches!(
            caps.network,
            NetworkPolicy::Denied | NetworkPolicy::Allowlist(_)
        )
    {
        return Err(unmet("sandbox.network", "it does not deny network access"));
    }
    if !plan.allow_write.is_empty() && !caps.honours_allow_write {
        return Err(unmet(
            "sandbox.allowWrite",
            "it does not enforce writable paths",
        ));
    }
    if plan.deny_home_reads && !caps.confines_reads {
        return Err(unmet("sandbox.denyHomeReads", "it does not confine reads"));
    }
    if !caps.confines_writes {
        return Err(unmet("sandbox.mode", "it does not confine writes"));
    }
    if !caps.protected_paths_enforced {
        return Err(unmet(
            "protected paths",
            "it does not protect .loop and git paths",
        ));
    }
    Ok(())
}

/// Registry with every built-in Workspace driver, then the drivers other
/// crates registered with `register_workspace_driver`. Each driver states
/// the platforms it supports; [`SandboxBuilder::start`] checks that at run time.
pub fn platform_registry() -> SandboxRegistry {
    let registry = SandboxRegistry::new();
    for driver in builtin_workspace_drivers() {
        registry.register_builtin(driver);
    }
    registry.with_registered_drivers()
}

/// Builds the platform's local sandbox from a [`SandboxRequest`].
pub struct SandboxBuilder {
    platform: SandboxPlatform,
    registry: SandboxRegistry,
    agent_dir: PathBuf,
}

impl SandboxBuilder {
    /// Builder for `platform` over `registry` (tests inject both).
    pub fn new(platform: SandboxPlatform, registry: SandboxRegistry) -> Self {
        Self {
            platform,
            registry,
            agent_dir: crate::config::paths::get_agent_dir(),
        }
    }

    /// Builder for the platform this binary runs on.
    pub fn for_current_platform() -> Self {
        Self::new(SandboxPlatform::current(), platform_registry())
    }

    /// Use the actual agent directory owned by this Runtime or standalone app.
    pub fn with_agent_dir(mut self, agent_dir: impl Into<PathBuf>) -> Self {
        self.agent_dir = agent_dir.into();
        self
    }

    /// Platform this builder targets.
    pub fn platform(&self) -> SandboxPlatform {
        self.platform
    }

    /// Apply platform defaults, rejecting explicit values the platform cannot honour.
    pub fn resolve(&self, request: &SandboxRequest) -> Result<SandboxPlan, SandboxBuildError> {
        let platform = self.platform;
        if platform != SandboxPlatform::Linux {
            if let Some(runtime) = request.runtime {
                return Err(SandboxBuildError::LinuxOnlyRuntime { platform, runtime });
            }
            if request.isolation == Some(KrunIsolation::Full) {
                return Err(SandboxBuildError::FullIsolationUnavailable { platform });
            }
        }
        Ok(SandboxPlan {
            isolation: request
                .isolation
                .unwrap_or_else(|| platform.default_isolation()),
            runtime: request.runtime.or_else(|| platform.default_runtime()),
            network: request.network,
            allow_write: request.allow_write.clone(),
            deny_home_reads: request.deny_home_reads,
            allow_read: request.allow_read.clone(),
        })
    }

    /// Resolve, create and start the sandbox for `workdir`.
    pub async fn start(
        &self,
        workdir: &Path,
        request: &SandboxRequest,
    ) -> Result<(Arc<dyn Workspace>, SandboxPlan), SandboxBuildError> {
        let plan = self.resolve(request)?;
        // Settings -> registry lookup -> platform check. A kind with no driver
        // for this platform is "not available", as before.
        let kind = request.kind.as_deref().unwrap_or(LOCAL_SANDBOX_KIND);
        let driver = self
            .registry
            .select(kind, self.platform.os())
            .map_err(|_| SandboxBuildError::Unavailable {
                platform: self.platform,
            })?;
        check_driver(driver.as_ref(), kind, request, &plan)?;
        let workspace = driver
            .create(plan.config_for_agent(workdir, &self.agent_dir).into())
            .await
            .map_err(|e| SandboxBuildError::Start(e.to_string()))?;
        let sandbox = workspace;
        if let Err(e) = sandbox.start().await {
            let _ = sandbox.destroy().await;
            return Err(SandboxBuildError::Start(e.to_string()));
        }
        Ok((sandbox, plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use loop_agent::harness::{
        ExecutionEnv, HostExecutionEnv, Sandbox, SandboxError, SandboxFactory, SandboxInfo,
        SandboxStatus,
    };
    use parking_lot::Mutex;

    struct FakeSandbox {
        env: Arc<dyn ExecutionEnv>,
        fail_start: bool,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn kind(&self) -> &str {
            LOCAL_SANDBOX_KIND
        }
        fn id(&self) -> &str {
            "fake"
        }
        fn status(&self) -> SandboxStatus {
            SandboxStatus::Ready
        }
        fn info(&self) -> SandboxInfo {
            SandboxInfo::enabled("fake", vec![])
        }
        fn env(&self) -> Arc<dyn ExecutionEnv> {
            Arc::clone(&self.env)
        }
        async fn start(&self) -> Result<(), SandboxError> {
            if self.fail_start {
                Err(SandboxError::StartFailed("podman missing".into()))
            } else {
                Ok(())
            }
        }
        async fn stop(&self) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn destroy(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeFactory {
        configs: Mutex<Vec<SandboxConfig>>,
        fail_start: bool,
    }

    #[async_trait]
    impl SandboxFactory for FakeFactory {
        fn kind(&self) -> &str {
            LOCAL_SANDBOX_KIND
        }
        async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
            let env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&config.workdir));
            self.configs.lock().push(config);
            Ok(Arc::new(FakeSandbox {
                env,
                fail_start: self.fail_start,
            }))
        }
    }

    fn builder_with(
        platform: SandboxPlatform,
        factory: Option<Arc<FakeFactory>>,
    ) -> SandboxBuilder {
        let registry = SandboxRegistry::new();
        if let Some(factory) = factory {
            registry.register(factory);
        }
        SandboxBuilder::new(platform, registry)
    }

    /// A driver with chosen capabilities over the fake factory.
    fn caps_builder(
        kind: &str,
        caps: loop_agent::harness::sandbox::WorkspaceCapabilities,
    ) -> SandboxBuilder {
        struct CapsFactory(String);
        #[async_trait]
        impl SandboxFactory for CapsFactory {
            fn kind(&self) -> &str {
                &self.0
            }
            async fn create(
                &self,
                config: SandboxConfig,
            ) -> Result<Arc<dyn Sandbox>, SandboxError> {
                FakeFactory::default().create(config).await
            }
        }
        static CAPS: Mutex<Option<loop_agent::harness::sandbox::WorkspaceCapabilities>> =
            Mutex::new(None);
        *CAPS.lock() = Some(caps);
        let registry = SandboxRegistry::new();
        registry.register_builtin(Arc::new(
            loop_agent::harness::sandbox::FactoryDriver::new(Arc::new(CapsFactory(kind.into())))
                .with_id("fake")
                .with_capabilities(|_| CAPS.lock().clone().unwrap_or_default()),
        ));
        SandboxBuilder::new(SandboxPlatform::Linux, registry)
    }

    fn guest_caps() -> loop_agent::harness::sandbox::WorkspaceCapabilities {
        loop_agent::harness::sandbox::WorkspaceCapabilities {
            path_namespace: PathNamespace::Guest,
            network: NetworkPolicy::Denied,
            confines_reads: true,
            confines_writes: true,
            protected_paths_enforced: true,
            honours_allow_write: true,
            ..Default::default()
        }
    }

    // One test owns the static capabilities above, so they do not race.
    #[tokio::test]
    async fn selection_refuses_drivers_that_relax_settings() {
        let dir = tempfile::tempdir().unwrap();
        let start = |builder: SandboxBuilder, request: SandboxRequest| {
            let path = dir.path().to_path_buf();
            async move { builder.start(&path, &request).await.map(|_| ()) }
        };

        // A guest-namespace driver with the tool lock (unknown counts).
        for policy in [PathPolicyUse::Unknown, PathPolicyUse::ToolLock] {
            let err = start(
                caps_builder("local", guest_caps()),
                SandboxRequest {
                    path_policy: policy,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                "the `fake` workspace driver does not meet the tool lock: its root and its \
                 confined reads are Runtime host paths, and this driver's paths are not host paths",
                "{policy:?}"
            );
        }
        // Path rules: refused until the guest leftovers are done (section 6).
        let err = start(
            caps_builder("local", guest_caps()),
            SandboxRequest {
                path_policy: PathPolicyUse::Rules,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the `fake` workspace driver does not meet path permission rules: some of their \
             checks still use Runtime host paths, and this driver's paths are not host paths"
        );
        // No path policy at all: a guest driver is fine.
        let request = SandboxRequest {
            path_policy: PathPolicyUse::None,
            ..Default::default()
        };
        start(caps_builder("local", guest_caps()), request)
            .await
            .unwrap();
        let no_policy = SandboxRequest {
            path_policy: PathPolicyUse::None,
            ..Default::default()
        };

        // Remote kind: every restriction must hold.
        let remote = |r: SandboxRequest| SandboxRequest {
            kind: Some("remote".into()),
            ..r
        };
        start(
            caps_builder("remote", guest_caps()),
            remote(no_policy.clone()),
        )
        .await
        .unwrap();
        let open_net = loop_agent::harness::sandbox::WorkspaceCapabilities {
            network: NetworkPolicy::NotEnforced,
            ..guest_caps()
        };
        let err = start(
            caps_builder("remote", open_net.clone()),
            remote(no_policy.clone()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the `fake` workspace driver does not meet sandbox.network: it does not deny network access"
        );
        let no_allow_write = loop_agent::harness::sandbox::WorkspaceCapabilities {
            honours_allow_write: false,
            ..guest_caps()
        };
        let with_allow_write = SandboxRequest {
            allow_write: vec!["x".into()],
            ..remote(no_policy.clone())
        };
        assert!(matches!(
            start(caps_builder("remote", no_allow_write), with_allow_write).await,
            Err(SandboxBuildError::Unmet {
                setting: "sandbox.allowWrite",
                ..
            })
        ));

        // Local kind with host paths keeps today's behaviour: no refusal.
        let host_open = loop_agent::harness::sandbox::WorkspaceCapabilities {
            path_namespace: PathNamespace::Host,
            ..open_net
        };
        start(caps_builder("local", host_open), SandboxRequest::default())
            .await
            .unwrap();
    }

    fn flags(args: &[&str]) -> SandboxRequest {
        SandboxRequest::for_flags(
            &SandboxSettings::default(),
            parse_local_sandbox_flags(args).unwrap(),
        )
    }

    #[test]
    fn flags_left_out_stay_unset() {
        assert_eq!(
            parse_local_sandbox_flags(&[]).unwrap(),
            LocalSandboxFlags::default()
        );
        let parsed = parse_local_sandbox_flags(&["--partial", "--krun"]).unwrap();
        assert_eq!(parsed.isolation, Some(KrunIsolation::Partial));
        assert_eq!(parsed.runtime, Some(LocalSandboxRuntime::Krun));
        let parsed = parse_local_sandbox_flags(&["full", "gvisor"]).unwrap();
        assert_eq!(parsed.isolation, Some(KrunIsolation::Full));
        assert_eq!(parsed.runtime, Some(LocalSandboxRuntime::Runsc));
        assert!(parse_local_sandbox_flags(&["--crun", "--krun"]).is_err());
        assert!(parse_local_sandbox_flags(&["--full", "--partial"]).is_err());
        assert!(parse_local_sandbox_flags(&["--docker"]).is_err());
    }

    #[test]
    fn linux_defaults_are_full_runc() {
        let builder = builder_with(SandboxPlatform::Linux, None);
        let plan = builder.resolve(&SandboxRequest::default()).unwrap();
        assert_eq!(plan.isolation, KrunIsolation::Full);
        assert_eq!(plan.runtime, Some(LocalSandboxRuntime::Runc));
        assert_eq!(plan.label(), "--full --runc");
    }

    #[test]
    fn linux_accepts_every_flag() {
        let builder = builder_with(SandboxPlatform::Linux, None);
        for args in [
            &["--full", "--runc"][..],
            &["--partial", "--crun"],
            &["--runsc"],
            &["--krun"],
        ] {
            assert!(builder.resolve(&flags(args)).is_ok(), "{args:?}");
        }
        let plan = builder.resolve(&flags(&["--partial", "--krun"])).unwrap();
        assert_eq!(plan.label(), "--partial --krun");
    }

    #[test]
    fn macos_defaults_are_partial_without_runtime() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let plan = builder.resolve(&SandboxRequest::default()).unwrap();
        assert_eq!(plan.isolation, KrunIsolation::Partial);
        assert_eq!(plan.runtime, None);
        assert_eq!(plan.label(), "--partial");
        assert!(builder.resolve(&flags(&["--partial"])).is_ok());
    }

    #[test]
    fn macos_rejects_explicit_linux_only_flags() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        for flag in ["--runc", "--crun", "--runsc", "--krun"] {
            let err = builder.resolve(&flags(&[flag])).unwrap_err();
            assert!(
                matches!(err, SandboxBuildError::LinuxOnlyRuntime { .. }),
                "{flag}: {err:?}"
            );
            let message = err.to_string();
            assert!(message.contains(flag), "{message}");
            assert!(message.contains("macOS"), "{message}");
        }
        let err = builder.resolve(&flags(&["--full"])).unwrap_err();
        assert_eq!(
            err,
            SandboxBuildError::FullIsolationUnavailable {
                platform: SandboxPlatform::MacOs
            }
        );
        assert!(err.to_string().contains("--full"));
    }

    #[test]
    fn macos_rejects_explicit_linux_only_settings_but_not_inherited_ones() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let inherited = SandboxSettings {
            mode: "local".into(),
            ..Default::default()
        };
        let request = SandboxRequest::from_settings(&inherited).unwrap();
        assert!(builder.resolve(&request).is_ok());

        let explicit = SandboxSettings {
            runtime: Some("runc".into()),
            ..inherited.clone()
        };
        let request = SandboxRequest::from_settings(&explicit).unwrap();
        let err = builder.resolve(&request).unwrap_err();
        assert!(err.to_string().contains("sandbox.runtime"), "{err}");

        let explicit = SandboxSettings {
            isolation: Some("full".into()),
            ..inherited
        };
        let request = SandboxRequest::from_settings(&explicit).unwrap();
        assert!(matches!(
            builder.resolve(&request),
            Err(SandboxBuildError::FullIsolationUnavailable { .. })
        ));
    }

    fn local_settings(isolation: Option<&str>, runtime: Option<&str>) -> SandboxRequest {
        SandboxRequest::from_settings(&SandboxSettings {
            mode: "local".into(),
            isolation: isolation.map(Into::into),
            runtime: runtime.map(Into::into),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn legacy_full_runc_pair_is_read_as_unset() {
        let request = local_settings(Some("full"), Some("runc"));
        assert_eq!(request.isolation, None);
        assert_eq!(request.runtime, None);

        let mac = builder_with(SandboxPlatform::MacOs, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(mac.label(), "--partial");
        let linux = builder_with(SandboxPlatform::Linux, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(linux.label(), "--full --runc");
    }

    #[test]
    fn only_the_exact_legacy_pair_is_forgiven_on_macos() {
        let mac = builder_with(SandboxPlatform::MacOs, None);
        for (isolation, runtime) in [
            (None, Some("runc")),
            (Some("full"), None),
            (Some("full"), Some("krun")),
            (Some("partial"), Some("runc")),
            (Some("full"), Some("crun")),
        ] {
            let request = local_settings(isolation, runtime);
            assert!(
                mac.resolve(&request).is_err(),
                "{isolation:?} {runtime:?} should be explicit"
            );
        }
    }

    #[test]
    fn typed_full_runc_still_errors_on_macos() {
        let mac = builder_with(SandboxPlatform::MacOs, None);
        let settings = SandboxSettings {
            mode: "local".into(),
            isolation: Some("full".into()),
            runtime: Some("runc".into()),
            ..Default::default()
        };
        for args in [&["--full", "--runc"][..], &["--runc"], &["--full"]] {
            let request =
                SandboxRequest::for_flags(&settings, parse_local_sandbox_flags(args).unwrap());
            assert!(mac.resolve(&request).is_err(), "{args:?}");
        }
    }

    #[test]
    fn unknown_setting_values_are_errors() {
        let settings = SandboxSettings {
            runtime: Some("docker".into()),
            ..Default::default()
        };
        let err = SandboxRequest::from_settings(&settings).unwrap_err();
        assert!(err.to_string().contains("sandbox.runtime"), "{err}");
        let settings = SandboxSettings {
            isolation: Some("half".into()),
            ..Default::default()
        };
        assert!(SandboxRequest::from_settings(&settings).is_err());
    }

    #[test]
    fn flags_replace_saved_isolation_and_runtime_but_keep_network() {
        let settings = SandboxSettings {
            isolation: Some("partial".into()),
            runtime: Some("docker".into()),
            network: Some(true),
            allow_write: vec!["~/.cargo".into()],
            ..Default::default()
        };
        let request = SandboxRequest::for_flags(&settings, LocalSandboxFlags::default());
        assert_eq!(request.isolation, None);
        assert_eq!(request.runtime, None);
        assert!(request.network);
        let home = dirs::home_dir().unwrap();
        assert_eq!(request.allow_write, vec![home.join(".cargo")]);

        // A bad saved runtime does not block the flags that replace it.
        let request =
            SandboxRequest::for_flags(&settings, parse_local_sandbox_flags(&["--crun"]).unwrap());
        let plan = builder_with(SandboxPlatform::Linux, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(plan.label(), "--full --crun");
    }

    #[tokio::test]
    async fn linux_builds_from_the_registered_local_factory() {
        let factory = Arc::new(FakeFactory::default());
        let builder = builder_with(SandboxPlatform::Linux, Some(Arc::clone(&factory)));
        let workdir = std::env::temp_dir();
        let absolute = workdir.join("cache");
        let request = SandboxRequest {
            network: true,
            allow_write: vec![absolute.clone(), "target".into()],
            ..flags(&["--partial", "--crun"])
        };
        let (sandbox, plan) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&workdir, &request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(sandbox.kind(), LOCAL_SANDBOX_KIND);
        assert_eq!(plan.label(), "--partial --crun");
        let configs = factory.configs.lock();
        assert_eq!(configs.len(), 1);
        let options = &configs[0].options;
        assert_eq!(configs[0].workdir, workdir);
        assert_eq!(options["isolation"], "partial");
        assert_eq!(options["runtime"], "crun");
        assert_eq!(options["network"], true);
        assert_eq!(
            options["allowWrite"],
            json!([
                absolute.display().to_string(),
                workdir.join("target").display().to_string()
            ])
        );
    }

    #[tokio::test]
    async fn missing_platform_factory_says_not_available() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &SandboxRequest::default()),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(
            err,
            SandboxBuildError::Unavailable {
                platform: SandboxPlatform::MacOs
            }
        );
        assert!(err.to_string().contains("not available on macOS yet"));
    }

    #[tokio::test]
    async fn explicit_flag_error_wins_over_missing_factory() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &flags(&["--krun"])),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert!(matches!(err, SandboxBuildError::LinuxOnlyRuntime { .. }));
    }

    #[tokio::test]
    async fn start_failure_is_reported() {
        let factory = Arc::new(FakeFactory {
            fail_start: true,
            ..Default::default()
        });
        let builder = builder_with(SandboxPlatform::Linux, Some(factory));
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &SandboxRequest::default()),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert!(err.to_string().contains("podman missing"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn platform_builder_starts_seatbelt_with_the_actual_agent_dir() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let agent = root.path().join("custom-agent");
            std::fs::create_dir(&agent).unwrap();
            std::fs::write(agent.join("auth.json"), "private").unwrap();
            let builder = SandboxBuilder::for_current_platform().with_agent_dir(&agent);
            let (sandbox, plan) = builder
                .start(root.path(), &SandboxRequest::default())
                .await
                .unwrap();
            assert_eq!(plan.isolation, KrunIsolation::Partial);
            assert!(sandbox.info().format_box().contains("seatbelt"));
            assert!(sandbox
                .env()
                .read_text_file(&agent.join("auth.json"))
                .await
                .is_err());
            sandbox.destroy().await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn platform_registry_matches_the_build_target() {
        let registry = platform_registry();
        let local = registry.select(LOCAL_SANDBOX_KIND, HostOs::current());
        assert_eq!(
            local.is_ok(),
            cfg!(any(target_os = "linux", target_os = "macos")),
            "{:?}",
            registry.driver_ids()
        );
        let expected = if cfg!(target_os = "linux") {
            Some("podman")
        } else if cfg!(target_os = "macos") {
            Some("seatbelt")
        } else {
            None
        };
        assert_eq!(local.ok().map(|d| d.id().to_owned()).as_deref(), expected);
    }

    #[test]
    fn platform_registry_selects_by_data_on_every_platform() {
        let registry = platform_registry();
        let pick = |os| {
            registry
                .select(LOCAL_SANDBOX_KIND, os)
                .ok()
                .map(|d| d.id().to_owned())
        };
        assert_eq!(pick(HostOs::Linux).as_deref(), Some("podman"));
        #[cfg(unix)]
        assert_eq!(pick(HostOs::MacOs).as_deref(), Some("seatbelt"));
        assert_eq!(pick(HostOs::Other), None);
    }

    #[tokio::test]
    async fn other_platform_with_the_full_registry_says_not_available() {
        let builder = SandboxBuilder::new(SandboxPlatform::Other, platform_registry());
        let dir = tempfile::tempdir().unwrap();
        let err = match builder.start(dir.path(), &SandboxRequest::default()).await {
            Err(e) => e,
            Ok(_) => panic!("started a sandbox on an unsupported platform"),
        };
        assert_eq!(
            err,
            SandboxBuildError::Unavailable {
                platform: SandboxPlatform::Other
            }
        );
        assert_eq!(
            err.to_string(),
            "the local sandbox is not available on this platform yet; tools run on the host (/sandbox off)."
        );
    }
}
