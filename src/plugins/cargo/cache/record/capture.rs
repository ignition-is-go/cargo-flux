//! Receipts emitted by the compiler producer, independent of Cargo's mutable aliases.
use super::*;
use std::ffi::OsString;

const CONFIG: &str = "CARGO_FLUX_COMPILER_CAPTURE";

#[derive(Serialize, Deserialize)]
struct Configuration {
    wrapper: Option<OsString>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Receipt {
    pub(super) manifest: PathBuf,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) out_dir: PathBuf,
    pub(super) hash: String,
}

pub(super) struct Capture(tempfile::TempDir);

impl Capture {
    pub(super) fn prepare(
        command: &mut Command,
        cwd: &Path,
        variables: &BTreeMap<String, String>,
        args: &[String],
    ) -> Option<Self> {
        if std::env::var_os(CONFIG).is_some() {
            return None;
        }
        for key in [
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC",
        ] {
            if variables.contains_key(key) || std::env::var_os(key).is_some() {
                return None;
            }
        }
        let workspace_wrapper = variables
            .get("RUSTC_WORKSPACE_WRAPPER")
            .map(OsString::from)
            .or_else(|| std::env::var_os("RUSTC_WORKSPACE_WRAPPER"));
        if workspace_wrapper.is_some_and(|wrapper| !wrapper.is_empty()) {
            return None;
        }
        // Configured compiler wrappers retain Cargo's own resolution semantics.
        // Environment wrappers can be forwarded exactly by this adapter.
        let mut configs = cwd
            .ancestors()
            .flat_map(|directory| {
                [
                    directory.join(".cargo/config"),
                    directory.join(".cargo/config.toml"),
                ]
            })
            .collect::<Vec<_>>();
        let home = variables
            .get("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
            .or_else(|| {
                #[cfg(windows)]
                let home = std::env::var_os("USERPROFILE");
                #[cfg(not(windows))]
                let home = std::env::var_os("HOME");
                home.map(|home| PathBuf::from(home).join(".cargo"))
            });
        if let Some(home) = home {
            let home = cwd.join(home);
            configs.extend([home.join("config"), home.join("config.toml")]);
        }
        if args
            .iter()
            .any(|arg| arg == "--config" || arg.starts_with("--config="))
        {
            return None;
        }
        for config in configs {
            let contents = match fs::read_to_string(config) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            let parsed: toml::Value = toml::from_str(&contents).ok()?;
            let build = parsed.get("build").and_then(toml::Value::as_table);
            let compiler_env = parsed
                .get("env")
                .and_then(toml::Value::as_table)
                .is_some_and(|env| {
                    [
                        "RUSTC",
                        "RUSTC_WRAPPER",
                        "RUSTC_WORKSPACE_WRAPPER",
                        "CARGO_BUILD_RUSTC",
                        "CARGO_BUILD_RUSTC_WRAPPER",
                        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
                    ]
                    .iter()
                    .any(|key| env.contains_key(*key))
                });
            if compiler_env
                || parsed.get("include").is_some()
                || build.is_some_and(|build| {
                    ["rustc-wrapper", "rustc-workspace-wrapper", "rustc"]
                        .iter()
                        .any(|key| build.contains_key(*key))
                })
            {
                return None;
            }
        }
        let compiler = variables
            .get("RUSTC")
            .map(OsString::from)
            .or_else(|| std::env::var_os("RUSTC"));
        if compiler.is_some_and(|compiler| {
            Path::new(&compiler)
                .file_stem()
                .is_none_or(|name| name != "rustc")
        }) {
            return None;
        }
        let wrapper = variables
            .get("RUSTC_WRAPPER")
            .map(OsString::from)
            .or_else(|| std::env::var_os("RUSTC_WRAPPER"));
        let directory = tempfile::tempdir().ok()?;
        let config = directory.path().join("config.json");
        fs::write(
            &config,
            serde_json::to_vec(&Configuration { wrapper }).ok()?,
        )
        .ok()?;
        command
            .env(CONFIG, &config)
            .env("RUSTC_WRAPPER", std::env::current_exe().ok()?);
        Some(Self(directory))
    }

    pub(super) fn receipts(&self) -> Result<Vec<Receipt>> {
        fs::read_dir(self.0.path())?
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_name() == "config.json" => None,
                entry => Some(entry),
            })
            .map(|entry| Ok(serde_json::from_reader(fs::File::open(entry?.path())?)?))
            .collect()
    }
}

pub(crate) fn compiler_entry() -> Result<()> {
    let Some(config) = std::env::var_os(CONFIG) else {
        return Ok(());
    };
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let Some(compiler) = args.first() else {
        return Ok(());
    };
    if Path::new(compiler)
        .file_stem()
        .is_none_or(|name| name != "rustc")
    {
        return Ok(());
    }
    let configuration: Configuration = serde_json::from_reader(fs::File::open(&config)?)?;
    let mut command = match configuration.wrapper {
        Some(wrapper) if !wrapper.is_empty() => {
            let mut command = Command::new(&wrapper);
            command.arg(compiler).env("RUSTC_WRAPPER", wrapper);
            command
        }
        wrapper => {
            let mut command = Command::new(compiler);
            if let Some(wrapper) = wrapper {
                command.env("RUSTC_WRAPPER", wrapper);
            } else {
                command.env_remove("RUSTC_WRAPPER");
            }
            command
        }
    };
    command.args(&args[1..]).env_remove(CONFIG);
    let status = command.status()?;
    if status.success()
        && let Err(error) = save_receipt(Path::new(&config), &args[1..])
    {
        eprintln!("compiler provenance unavailable: {error}");
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn save_receipt(config: &Path, args: &[OsString]) -> Result<()> {
    let Some(directory) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return Ok(());
    };
    let manifest = PathBuf::from(directory).join("Cargo.toml").canonicalize()?;
    let name = std::env::var("CARGO_PKG_NAME")?;
    let version = std::env::var("CARGO_PKG_VERSION")?;
    let mut out_dir = None;
    let mut hash = None;
    for (index, arg) in args.iter().enumerate() {
        if arg == "--out-dir" {
            out_dir = args.get(index + 1).map(PathBuf::from);
        }
        let option = if arg == "-C" {
            args.get(index + 1).and_then(|arg| arg.to_str())
        } else {
            arg.to_str().and_then(|arg| arg.strip_prefix("-C"))
        };
        if let Some(value) = option.and_then(|arg| arg.strip_prefix("extra-filename=-")) {
            hash = Some(value.to_owned());
        }
    }
    let Some(out_dir) = out_dir else {
        return Ok(());
    };
    let out_dir = out_dir.canonicalize()?;
    let hash = hash.or_else(|| {
        // Cargo 1.100 places each unit in build/package/hash/out.
        if out_dir.file_name()? == "out" {
            let unit = out_dir.parent()?;
            if unit.parent()?.parent()?.file_name()? == "build" {
                return unit.file_name()?.to_str().map(str::to_owned);
            }
        }
        out_dir
            .file_name()?
            .to_str()
            .and_then(artifact_hash)
            .map(str::to_owned)
    });
    let Some(hash) =
        hash.filter(|hash| hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    else {
        return Ok(());
    };
    let mut file = tempfile::NamedTempFile::new_in(config.parent().context("capture directory")?)?;
    serde_json::to_writer(
        file.as_file_mut(),
        &Receipt {
            manifest,
            name,
            version,
            out_dir,
            hash,
        },
    )?;
    file.keep()?;
    Ok(())
}
