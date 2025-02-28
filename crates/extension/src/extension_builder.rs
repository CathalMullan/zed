use crate::{
    parse_wasm_extension_version, ExtensionLibraryKind, ExtensionManifest, GrammarManifestEntry,
};
use anyhow::{anyhow, bail, Context as _, Result};
use http_client::{self, HttpClient};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

/// We compile with Rust's `wasm32-wasip2` target, which supports the WASM component model.
const RUST_TARGET: &str = "wasm32-wasip2";

pub struct ExtensionBuilder {
    cache_dir: PathBuf,
    pub http: Arc<dyn HttpClient>,
}

pub struct CompileExtensionOptions {
    pub release: bool,
}

#[derive(Deserialize)]
struct CargoToml {
    package: CargoTomlPackage,
}

#[derive(Deserialize)]
struct CargoTomlPackage {
    name: String,
}

impl ExtensionBuilder {
    pub fn new(http_client: Arc<dyn HttpClient>, cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            http: http_client,
        }
    }

    pub async fn compile_extension(
        &self,
        extension_dir: &Path,
        extension_manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
    ) -> Result<()> {
        populate_defaults(extension_manifest, extension_dir)?;

        if extension_dir.is_relative() {
            bail!(
                "extension dir {} is not an absolute path",
                extension_dir.display()
            );
        }

        fs::create_dir_all(&self.cache_dir).context("failed to create cache dir")?;

        if extension_manifest.lib.kind == Some(ExtensionLibraryKind::Rust) {
            log::info!("compiling Rust extension {}", extension_dir.display());
            self.compile_rust_extension(extension_dir, extension_manifest, options)
                .await
                .context("failed to compile Rust extension")?;
            log::info!("compiled Rust extension {}", extension_dir.display());
        }

        for (grammar_name, grammar_metadata) in &extension_manifest.grammars {
            log::info!(
                "compiling grammar {grammar_name} for extension {}",
                extension_dir.display()
            );
            self.compile_grammar(extension_dir, grammar_name.as_ref(), grammar_metadata)
                .await
                .with_context(|| format!("failed to compile grammar '{grammar_name}'"))?;
            log::info!(
                "compiled grammar {grammar_name} for extension {}",
                extension_dir.display()
            );
        }

        log::info!("finished compiling extension {}", extension_dir.display());
        Ok(())
    }

    async fn compile_rust_extension(
        &self,
        extension_dir: &Path,
        manifest: &mut ExtensionManifest,
        options: CompileExtensionOptions,
    ) -> Result<(), anyhow::Error> {
        let cargo_toml_content = fs::read_to_string(extension_dir.join("Cargo.toml"))?;
        let cargo_toml: CargoToml = toml::from_str(&cargo_toml_content)?;

        log::info!(
            "compiling Rust crate for extension {}",
            extension_dir.display()
        );
        let output = util::command::new_std_command("cargo")
            .args(["build", "--target", RUST_TARGET])
            .args(options.release.then_some("--release"))
            .arg("--target-dir")
            .arg(extension_dir.join("target"))
            // WASI builds do not work with sccache and just stuck, so disable it.
            .env("RUSTC_WRAPPER", "")
            .current_dir(extension_dir)
            .output()
            .context("failed to run `cargo`")?;
        if !output.status.success() {
            bail!(
                "failed to build extension {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        log::info!(
            "compiled Rust crate for extension {}",
            extension_dir.display()
        );

        let mut wasm_path = PathBuf::from(extension_dir);
        wasm_path.extend([
            "target",
            RUST_TARGET,
            if options.release { "release" } else { "debug" },
            &cargo_toml
                .package
                .name
                // The wasm32-wasip1 target normalizes `-` in package names to `_` in the resulting `.wasm` file.
                .replace('-', "_"),
        ]);
        wasm_path.set_extension("wasm");

        let component_bytes = fs::read(&wasm_path).with_context(|| {
            format!("failed to read component module `{}`", wasm_path.display())
        })?;

        let wasm_extension_api_version =
            parse_wasm_extension_version(&manifest.id, &component_bytes)
                .context("compiled wasm did not contain a valid zed extension api version")?;
        manifest.lib.version = Some(wasm_extension_api_version);

        let extension_file = extension_dir.join("extension.wasm");
        fs::write(extension_file.clone(), &component_bytes)
            .context("failed to write extension.wasm")?;

        log::info!(
            "extension {} written to {}",
            extension_dir.display(),
            extension_file.display()
        );

        Ok(())
    }

    async fn compile_grammar(
        &self,
        extension_dir: &Path,
        grammar_name: &str,
        grammar_metadata: &GrammarManifestEntry,
    ) -> Result<()> {
        let clang_path = which::which("clang")?;
        let wasi_libc_path =
            std::env::var("WASI_LIBC_PATH").expect("WASI_LIBC_PATH environment variable not set");

        let mut grammar_repo_dir = extension_dir.to_path_buf();
        grammar_repo_dir.extend(["grammars", grammar_name]);

        let mut grammar_wasm_path = grammar_repo_dir.clone();
        grammar_wasm_path.set_extension("wasm");

        log::info!("checking out {grammar_name} parser");
        self.checkout_repo(
            &grammar_repo_dir,
            &grammar_metadata.repository,
            &grammar_metadata.rev,
        )?;

        let base_grammar_path = grammar_metadata
            .path
            .as_ref()
            .map(|path| grammar_repo_dir.join(path))
            .unwrap_or(grammar_repo_dir);

        let src_path = base_grammar_path.join("src");
        let parser_path = src_path.join("parser.c");
        let scanner_path = src_path.join("scanner.c");

        log::info!("compiling {grammar_name} parser");
        let clang_output = util::command::new_std_command(&clang_path)
            .args(["--target=wasm32-wasi"])
            .args([format!("--sysroot={wasi_libc_path}")])
            .args(["-fPIC", "-shared", "-Os"])
            .arg(format!("-Wl,--export=tree_sitter_{grammar_name}"))
            .arg("-o")
            .arg(&grammar_wasm_path)
            .arg("-I")
            .arg(&src_path)
            .arg(&parser_path)
            .args(scanner_path.exists().then_some(scanner_path))
            .output()
            .context("failed to run clang")?;

        if !clang_output.status.success() {
            bail!(
                "failed to compile {} parser with clang: {}",
                grammar_name,
                String::from_utf8_lossy(&clang_output.stderr),
            );
        }

        Ok(())
    }

    fn checkout_repo(&self, directory: &Path, url: &str, rev: &str) -> Result<()> {
        if directory.exists() {
            return Ok(());
        }

        let git_dir = directory.join(".git");

        fs::create_dir_all(directory).with_context(|| {
            format!("failed to create grammar directory {}", directory.display(),)
        })?;
        let init_output = util::command::new_std_command("git")
            .arg("init")
            .current_dir(directory)
            .output()?;
        if !init_output.status.success() {
            bail!(
                "failed to run `git init` in directory '{}'",
                directory.display()
            );
        }

        let remote_add_output = util::command::new_std_command("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["remote", "add", "origin", url])
            .output()
            .context("failed to execute `git remote add`")?;
        if !remote_add_output.status.success() {
            bail!(
                "failed to add remote {url} for git repository {}",
                git_dir.display()
            );
        }

        let fetch_output = util::command::new_std_command("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["fetch", "--depth", "1", "origin", rev])
            .output()
            .context("failed to execute `git fetch`")?;

        let checkout_output = util::command::new_std_command("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["checkout", rev])
            .current_dir(directory)
            .output()
            .context("failed to execute `git checkout`")?;
        if !checkout_output.status.success() {
            if !fetch_output.status.success() {
                bail!(
                    "failed to fetch revision {} in directory '{}'",
                    rev,
                    directory.display()
                );
            }
            bail!(
                "failed to checkout revision {} in directory '{}': {}",
                rev,
                directory.display(),
                String::from_utf8_lossy(&checkout_output.stderr)
            );
        }

        Ok(())
    }
}

fn populate_defaults(manifest: &mut ExtensionManifest, extension_path: &Path) -> Result<()> {
    // For legacy extensions on the v0 schema (aka, using `extension.json`), clear out any existing
    // contents of the computed fields, since we don't care what the existing values are.
    if manifest.schema_version.is_v0() {
        manifest.languages.clear();
        manifest.grammars.clear();
        manifest.themes.clear();
    }

    let cargo_toml_path = extension_path.join("Cargo.toml");
    if cargo_toml_path.exists() {
        manifest.lib.kind = Some(ExtensionLibraryKind::Rust);
    }

    let languages_dir = extension_path.join("languages");
    if languages_dir.exists() {
        for entry in fs::read_dir(&languages_dir).context("failed to list languages dir")? {
            let entry = entry?;
            let language_dir = entry.path();
            let config_path = language_dir.join("config.toml");
            if config_path.exists() {
                let relative_language_dir =
                    language_dir.strip_prefix(extension_path)?.to_path_buf();
                if !manifest.languages.contains(&relative_language_dir) {
                    manifest.languages.push(relative_language_dir);
                }
            }
        }
    }

    let themes_dir = extension_path.join("themes");
    if themes_dir.exists() {
        for entry in fs::read_dir(&themes_dir).context("failed to list themes dir")? {
            let entry = entry?;
            let theme_path = entry.path();
            if theme_path.extension() == Some("json".as_ref()) {
                let relative_theme_path = theme_path.strip_prefix(extension_path)?.to_path_buf();
                if !manifest.themes.contains(&relative_theme_path) {
                    manifest.themes.push(relative_theme_path);
                }
            }
        }
    }

    let icon_themes_dir = extension_path.join("icon_themes");
    if icon_themes_dir.exists() {
        for entry in fs::read_dir(&icon_themes_dir).context("failed to list icon themes dir")? {
            let entry = entry?;
            let icon_theme_path = entry.path();
            if icon_theme_path.extension() == Some("json".as_ref()) {
                let relative_icon_theme_path =
                    icon_theme_path.strip_prefix(extension_path)?.to_path_buf();
                if !manifest.icon_themes.contains(&relative_icon_theme_path) {
                    manifest.icon_themes.push(relative_icon_theme_path);
                }
            }
        }
    }

    let snippets_json_path = extension_path.join("snippets.json");
    if snippets_json_path.exists() {
        manifest.snippets = Some(snippets_json_path);
    }

    // For legacy extensions on the v0 schema (aka, using `extension.json`), we want to populate the grammars in
    // the manifest using the contents of the `grammars` directory.
    if manifest.schema_version.is_v0() {
        let grammars_dir = extension_path.join("grammars");
        if grammars_dir.exists() {
            for entry in fs::read_dir(&grammars_dir).context("failed to list grammars dir")? {
                let entry = entry?;
                let grammar_path = entry.path();
                if grammar_path.extension() == Some("toml".as_ref()) {
                    #[derive(Deserialize)]
                    struct GrammarConfigToml {
                        pub repository: String,
                        pub commit: String,
                        #[serde(default)]
                        pub path: Option<String>,
                    }

                    let grammar_config = fs::read_to_string(&grammar_path)?;
                    let grammar_config: GrammarConfigToml = toml::from_str(&grammar_config)?;

                    let grammar_name = grammar_path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .ok_or_else(|| anyhow!("no grammar name"))?;
                    if !manifest.grammars.contains_key(grammar_name) {
                        manifest.grammars.insert(
                            grammar_name.into(),
                            GrammarManifestEntry {
                                repository: grammar_config.repository,
                                rev: grammar_config.commit,
                                path: grammar_config.path,
                            },
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
