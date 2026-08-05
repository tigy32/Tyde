use anyhow::{bail, Context, Result};
use bindgen::callbacks::{AttributeInfo, DeriveInfo, ParseCallbacks};
use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    process::Command,
};

mod build_support;
use build_support::{CommandSpec, SymbolTools};

/// Name and minimum version of the library that we are binding to.
const LIB_NAME: &str = "webrtc-audio-processing-2";
#[cfg(not(feature = "bundled"))]
const LIB_MIN_VERSION: &str = "2.1";

const MACOSX_DEPLOYMENT_TARGET_VAR: &str = "MACOSX_DEPLOYMENT_TARGET";

/// Symbol prefix for the webrtc-audio-processing library to allow multiple versions to coexist.
const SYMBOL_PREFIX: &str = "v2_";
#[cfg(feature = "bundled")]
const BUNDLED_ABSEIL_LINK_LIBRARIES: [&str; 1] = ["absl_strings"];

fn out_dir() -> PathBuf {
    std::env::var("OUT_DIR").expect("OUT_DIR environment var not set.").into()
}

fn repository_native_tool(name: &str) -> Command {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository = manifest
        .parent()
        .and_then(|vendor| vendor.parent())
        .expect("vendored native audio dependency must be inside the Tyde repository");
    let wrapper = repository.join("tools/native-build-tool.py");
    println!("cargo:rerun-if-changed={}", wrapper.display());
    println!(
        "cargo:rerun-if-changed={}",
        repository
            .join("tools/provision-native-build-tools.py")
            .display()
    );
    let python = env::var_os("PYTHON").unwrap_or_else(|| {
        if cfg!(windows) {
            "python".into()
        } else {
            "python3".into()
        }
    });
    let mut command = Command::new(python);
    command.arg(wrapper).arg(name).arg("--");
    command
}

fn repository_native_command(name: &str, spec: &CommandSpec) -> Command {
    let mut command = repository_native_tool(name);
    command.args(&spec.args).envs(spec.env.iter().cloned());
    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }
    command
}

/// Prefix specified symbols in a static library using objcopy --redefine-sym.
fn prefix_archive_symbols(
    archive_path: &std::path::Path,
    symbols: &[String],
    prefix: &str,
    target: &str,
    llvm_tools: Option<&SymbolTools>,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }

    eprintln!(
        "Prefixing {} symbols in {} with '{}'",
        symbols.len(),
        archive_path.display(),
        prefix
    );

    let temp_path = build_support::prefixed_archive_path(target, archive_path);
    let args_path = archive_path.with_extension("args");
    if temp_path.is_file() {
        std::fs::remove_file(&temp_path).with_context(|| {
            format!("removing stale prefixed archive {}", temp_path.display())
        })?;
    } else if temp_path.exists() {
        bail!("prefixed archive path is not a file: {}", temp_path.display());
    }
    let unix_objcopy = if build_support::is_msvc_target(target) {
        None
    } else {
        Some(determine_objcopy_path()?)
    };
    let spec = build_support::symbol_prefix_spec(
        target,
        llvm_tools,
        unix_objcopy.as_deref(),
        &args_path,
        archive_path,
        &temp_path,
    )
    .map_err(anyhow::Error::msg)?;

    // Write arguments to a temp file to avoid "Argument list too long" errors.
    let mut writer = BufWriter::new(File::create(&args_path)?);
    for symbol in symbols {
        writeln!(writer, "--redefine-sym={}={}{}", symbol, prefix, symbol)?;
    }
    writer.flush()?;
    drop(writer);

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);

    eprintln!("Running {cmd:?}");
    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute {:?}", spec.program))?;

    if !status.success() {
        anyhow::bail!("{:?} failed with status: {}", spec.program, status);
    }

    build_support::replace_with_prefixed_archive(target, &temp_path, archive_path)
        .map_err(anyhow::Error::msg)?;
    std::fs::remove_file(&args_path)
        .with_context(|| format!("removing symbol-prefix arguments {}", args_path.display()))?;

    Ok(())
}

#[cfg(not(feature = "bundled"))]
mod webrtc {
    use super::*;

    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let (pkgconfig_include_path, pkgconfig_lib_path) = find_pkgconfig_paths()?;

        let include_path = std::env::var("WEBRTC_AUDIO_PROCESSING_INCLUDE")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_include_path);
        let lib_path = std::env::var("WEBRTC_AUDIO_PROCESSING_LIB")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_lib_path);

        if include_path.is_none() || lib_path.is_none() {
            bail!(
                "Couldn't find {}. Please install it or set WEBRTC_AUDIO_PROCESSING_INCLUDE and WEBRTC_AUDIO_PROCESSING_LIB environment variables.",
                LIB_NAME
            );
        }

        Ok((vec![include_path.unwrap()], vec![lib_path.unwrap()]))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        Ok(())
    }

    fn find_pkgconfig_paths() -> Result<(Option<PathBuf>, Option<PathBuf>)> {
        let lib = match pkg_config::Config::new()
            .atleast_version(LIB_MIN_VERSION)
            .statik(false)
            .probe(LIB_NAME)
        {
            Ok(lib) => lib,
            Err(e) => {
                eprintln!("Couldn't find {LIB_NAME} with pkg-config:");
                eprintln!("{e}");
                return Ok((None, None));
            },
        };

        Ok((lib.include_paths.first().cloned(), lib.link_paths.first().cloned()))
    }

    pub(super) fn prefix_library_symbols(
        _lib_dirs: &[PathBuf],
        _prefix: &str,
        _llvm_tools: Option<&SymbolTools>,
    ) -> Result<Vec<String>> {
        // For non-bundled builds, we can't prefix symbols in the system library.
        // Users would need to build with bundled feature for multi-version support.
        println!(
            "cargo:warning=Symbol prefixing is only supported with the 'bundled' feature. \
            Without it, linking multiple versions of this crate may cause symbol conflicts."
        );

        Ok(vec![])
    }
}

#[cfg(feature = "bundled")]
mod webrtc {
    use super::*;
    use crate::build_support::MsvcTools;
    use std::{collections::HashSet, fs, path::Path};

    const BUNDLED_SOURCE_PATH: &str = "./webrtc-audio-processing";
    const ABSEIL_SUBPROJECT_DIRECTORY: &str = "abseil-cpp-20240722.0";
    const ABSEIL_PACKAGE_FILES: [&str; 6] = [
        "subprojects/abseil-cpp.wrap",
        "subprojects/packagecache/abseil-cpp-20240722.0.tar.gz",
        "subprojects/packagecache/abseil-cpp_20240722.0-3_patch.zip",
        "subprojects/packagecache/ABSEIL-LICENSE",
        "subprojects/packagecache/WRAPDB-LICENSE",
        "subprojects/packagecache/PROVENANCE.md",
    ];

    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut include_paths = vec![
            out_dir().join("include"),
            out_dir().join("include").join(LIB_NAME),
            webrtc_source_dir(),
            webrtc_source_dir().join("webrtc"),
        ];
        // TODO(strohel): instead of hardcoding the paths, we should consult the pkgconfig file that
        // the bundled webrtc-audio-processing build produces.
        let mut lib_paths = vec![
            // MacOS, Arch Linux, baseline default
            out_dir().join("lib"),
            // Ubuntu Linux (our CI)
            out_dir().join("lib").join("x86_64-linux-gnu"),
            // Ubuntu Linux (Arm 64bit)
            out_dir().join("lib").join("aarch64-linux-gnu"),
            // Gentoo Linux (x86_64 multilib)
            out_dir().join("lib64"),
        ];

        include_paths.push(
            webrtc_source_dir()
                .join("subprojects")
                .join(ABSEIL_SUBPROJECT_DIRECTORY),
        );
        lib_paths.push(
            webrtc_build_dir()
                .join("subprojects")
                .join(ABSEIL_SUBPROJECT_DIRECTORY),
        );

        Ok((include_paths, lib_paths))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        let bundled_source_path = Path::new(BUNDLED_SOURCE_PATH);
        if bundled_source_path.read_dir()?.next().is_none() {
            eprintln!("The webrtc-audio-processing source directory is empty.");
            eprintln!("See the crate README for installation instructions.");
            eprintln!("Remember to clone the repo recursively if building from source.");
            bail!("Aborting compilation because bundled source directory is empty.");
        }
        for package_file in ABSEIL_PACKAGE_FILES {
            let package_file = bundled_source_path.join(package_file);
            println!("cargo:rerun-if-changed={}", package_file.display());
            if !package_file.is_file() {
                bail!(
                    "bundled Abseil package-cache artifact is missing: {}",
                    package_file.display()
                );
            }
        }

        let webrtc_source_dir = webrtc_source_dir();
        let webrtc_build_dir = webrtc_build_dir();
        let target = env::var("TARGET").context("TARGET environment variable is not set")?;
        let msvc = resolve_msvc_tools(&target)?;
        eprintln!(
            "Copying webrtc-audio-processing to {} and building it in {}",
            webrtc_source_dir.display(),
            webrtc_build_dir.display()
        );

        // Copy the sources to under out directory so that we can patch it without consequences.
        let mut cp = Command::new("cp");
        // Copy recursively, preserve attributes. Use trailing dot trick to prevent creating
        // `webrtc-audio-processing/webrtc-audio-processing` nesting on a 2nd invocation.
        cp.arg("-a").arg(bundled_source_path.join(".")).arg(&webrtc_source_dir);
        let status = cp.status().context("executing cp")?;
        assert!(status.success(), "Command failed: {:?}", &cp);

        #[cfg(feature = "experimental-unlink-ns")]
        apply_patch("unlink-multichannel-noise-suppression-filters.patch")?;

        let coredata = webrtc_build_dir.join("meson-private/coredata.dat");
        if webrtc_build_dir.exists() && !coredata.is_file() {
            fs::remove_dir_all(&webrtc_build_dir).with_context(|| {
                format!(
                    "removing incomplete AEC build directory {}",
                    webrtc_build_dir.display()
                )
            })?;
        }
        remove_materialized_abseil(&webrtc_source_dir)?;
        let reconfigure = coredata.is_file();
        if !run_meson_setup(
            &webrtc_build_dir,
            &webrtc_source_dir,
            reconfigure,
            &target,
            msvc.as_ref(),
        )? {
            eprintln!("AEC Meson reconfigure failed; retrying once from clean target state");
            if webrtc_build_dir.exists() {
                fs::remove_dir_all(&webrtc_build_dir).with_context(|| {
                    format!(
                        "removing failed AEC build directory {}",
                        webrtc_build_dir.display()
                    )
                })?;
            }
            remove_materialized_abseil(&webrtc_source_dir)?;
            if !run_meson_setup(
                &webrtc_build_dir,
                &webrtc_source_dir,
                false,
                &target,
                msvc.as_ref(),
            )? {
                bail!("Meson could not configure the bundled AEC from its offline package cache");
            }
        }

        let msvc_env = msvc.as_ref().map(|tools| tools.env.as_slice());
        let ninja_spec = build_support::ninja_spec(&webrtc_build_dir, false, msvc_env);
        let mut ninja = repository_native_command("ninja", &ninja_spec);
        let status = ninja
            .status()
            .context("Failed to execute ninja. Do you have it installed?")?;
        assert!(status.success(), "Command failed: {:?}", &ninja);

        let install_spec = build_support::ninja_spec(&webrtc_build_dir, true, msvc_env);
        let mut install = repository_native_command("ninja", &install_spec);
        let status = install
            .status()
            .context("Failed to execute ninja install")?;
        assert!(status.success(), "Command failed: {:?}", &install);

        Ok(())
    }

    fn remove_materialized_abseil(source_dir: &Path) -> Result<()> {
        let materialized = source_dir
            .join("subprojects")
            .join(ABSEIL_SUBPROJECT_DIRECTORY);
        if materialized.exists() {
            fs::remove_dir_all(&materialized).with_context(|| {
                format!(
                    "removing materialized Abseil subproject {}",
                    materialized.display()
                )
            })?;
        }
        Ok(())
    }

    fn run_meson_setup(
        build_dir: &Path,
        source_dir: &Path,
        reconfigure: bool,
        target: &str,
        msvc: Option<&MsvcTools>,
    ) -> Result<bool> {
        let native_file = if let Some(tools) = msvc {
            let path = out_dir().join("webrtc-msvc-native.ini");
            fs::write(&path, build_support::msvc_native_file(tools))
                .with_context(|| format!("writing MSVC Meson native file {}", path.display()))?;
            Some(path)
        } else {
            None
        };
        let msvc_config = native_file
            .as_deref()
            .zip(msvc.map(|tools| tools.env.as_slice()));
        let spec = build_support::meson_spec(
            build_dir,
            source_dir,
            &out_dir(),
            reconfigure,
            target.contains("-apple-darwin"),
            msvc_config,
        );
        let mut meson = repository_native_command("meson", &spec);
        let status = meson
            .status()
            .context("Failed to execute Meson for the bundled AEC")?;
        Ok(status.success())
    }

    fn resolve_msvc_tools(target: &str) -> Result<Option<MsvcTools>> {
        if !target.ends_with("-pc-windows-msvc") {
            return Ok(None);
        }

        fn find(target: &str, name: &str) -> Result<cc::Tool> {
            cc::windows_registry::find_tool(target, name).with_context(|| {
                format!("could not resolve {name} for Rust MSVC target {target}")
            })
        }

        let compiler = find(target, "cl.exe")?;
        anyhow::ensure!(
            compiler.is_like_msvc(),
            "resolved compiler for {target} is not MSVC: {}",
            compiler.path().display()
        );
        let linker = find(target, "link.exe")?;
        let librarian = find(target, "lib.exe")?;
        Ok(Some(MsvcTools {
            compiler: compiler.path().to_owned(),
            linker: linker.path().to_owned(),
            librarian: librarian.path().to_owned(),
            env: build_support::msvc_command_env(compiler.env()),
        }))
    }

    // Patch with `patch`.
    #[cfg(feature = "experimental-unlink-ns")]
    fn apply_patch(patch_name: &str) -> Result<()> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let patch = manifest.join("patches").join(patch_name);

        let status = Command::new("patch")
            .args(["-p1", "--forward"])
            .arg("-i")
            .arg(&patch)
            .current_dir(webrtc_source_dir())
            .status()
            .context("Failed to execute patch")?;

        anyhow::ensure!(status.success(), "Patch '{}' failed with status: {}", patch_name, status);
        Ok(())
    }

    /// Prefix symbols in the built webrtc-audio-processing static library.
    /// Returns the renamed symbols and the staged archive link contract.
    pub(super) fn prefix_library_symbols(
        lib_dirs: &[PathBuf],
        prefix: &str,
        llvm_tools: Option<&SymbolTools>,
        staging_dir: &Path,
    ) -> Result<(Vec<String>, build_support::StagedArchive)> {
        let target = env::var("TARGET").context("TARGET environment variable is not set")?;
        let archive = build_support::discover_bundled_archive(&target, LIB_NAME, lib_dirs)
            .map_err(anyhow::Error::msg)?;
        let symbols = get_defined_symbols(&archive.source, &target, llvm_tools)?;
        let staged = build_support::stage_and_prepare_bundled_archive(
            &archive,
            staging_dir,
            &llvm_tools
                .context("bundled archive staging requires resolved Rust LLVM tools")?
                .archive,
            |staged_path| {
                prefix_archive_symbols(staged_path, &symbols, prefix, &target, llvm_tools)
                    .map_err(|error| error.to_string())
            },
        )
        .map_err(anyhow::Error::msg)?;
        Ok((symbols, staged))
    }

    fn webrtc_source_dir() -> PathBuf {
        out_dir().join(build_support::BUNDLED_SOURCE_DIRECTORY)
    }

    fn webrtc_build_dir() -> PathBuf {
        out_dir().join(build_support::BUNDLED_BUILD_DIRECTORY)
    }

    /// Extract defined (non-external) symbols from a static library using nm.
    fn get_defined_symbols(
        archive_path: &std::path::Path,
        target: &str,
        llvm_tools: Option<&SymbolTools>,
    ) -> Result<Vec<String>> {
        let spec = build_support::symbol_list_spec(target, llvm_tools, archive_path)
            .map_err(anyhow::Error::msg)?;
        let output = Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .with_context(|| format!("Failed to execute {:?}", spec.program))?;

        if !output.status.success() {
            anyhow::bail!(
                "{:?} failed: {}",
                spec.program,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut symbols = HashSet::new();

        for line in stdout.lines() {
            // POSIX format: "symbol_name type value size"
            // We just need the first field (symbol name)
            if let Some(symbol) = line.split_whitespace().next() {
                symbols.insert(symbol.to_string());
            }
        }

        Ok(symbols.into_iter().collect())
    }
}

#[derive(Debug)]
struct CustomDeriveCallbacks;

impl ParseCallbacks for CustomDeriveCallbacks {
    fn add_derives(&self, info: &DeriveInfo) -> Vec<String> {
        // Matches EchoCanceller3Config, EchoCanceller3Config_Suppressor etc
        if info.name.starts_with("EchoCanceller3Config") && cfg!(feature = "serde") {
            vec!["serde::Deserialize".into(), "serde::Serialize".into()]
        // Matches AudioProcessing_Config, AudioProcessing_Config_EchoCanceller etc
        } else if info.name.starts_with("AudioProcessing_Config") {
            // Only derive Default for AudioProcessing_Config and its inner structs. bindgen Default
            // implementation ignores C/C++ struct default values and thus misleading to enable
            // globally. Note that we don't expose these defaults on `webrtc-audio-processing`
            // level: they are needed only by the code that converts from prettified Rust config
            // structs into their FFI variants to construct disabled/dummy values.
            vec!["Default".into()]
        } else {
            vec![]
        }
    }

    fn add_attributes(&self, info: &AttributeInfo<'_>) -> Vec<String> {
        if info.name.starts_with("EchoCanceller3Config") {
            // Prohibit construction of ffi EchoCanceller3Config and its children structs.
            // The only allowed API is through the wrapper struct in the webrtc_audio_processing crate.
            vec!["#[non_exhaustive]".into()]
        } else {
            vec![]
        }
    }
}

fn main() -> Result<()> {
    let target = env::var("TARGET").context("TARGET environment variable is not set")?;
    webrtc::build_if_necessary()?;
    let (include_dirs, lib_dirs) = webrtc::get_build_paths()?;
    #[cfg(feature = "bundled")]
    let staging_dir = out_dir().join(build_support::BUNDLED_LINK_DIRECTORY);

    #[cfg(feature = "bundled")]
    let llvm_tools = Some(determine_llvm_tools()?);
    #[cfg(not(feature = "bundled"))]
    let llvm_tools: Option<SymbolTools> = None;

    // Prefix defined symbols in the webrtc library (bundled builds only)
    // Returns the list of renamed symbols to update wrapper references later
    #[cfg(feature = "bundled")]
    let (renamed_symbols, bundled_archive) =
        webrtc::prefix_library_symbols(
            &lib_dirs,
            SYMBOL_PREFIX,
            llvm_tools.as_ref(),
            &staging_dir,
        )?;
    #[cfg(feature = "bundled")]
    let bundled_abseil_archives = BUNDLED_ABSEIL_LINK_LIBRARIES
        .iter()
        .copied()
        .map(|name| {
            let archive = build_support::discover_bundled_archive(&target, name, &lib_dirs)
                .map_err(anyhow::Error::msg)?;
            build_support::stage_bundled_archive(
                &archive,
                &staging_dir,
                &llvm_tools
                    .as_ref()
                    .context("bundled archive staging requires resolved Rust LLVM tools")?
                    .archive,
            )
            .map_err(anyhow::Error::msg)
        })
        .collect::<Result<Vec<_>>>()?;
    #[cfg(not(feature = "bundled"))]
    let renamed_symbols =
        webrtc::prefix_library_symbols(&lib_dirs, SYMBOL_PREFIX, llvm_tools.as_ref())?;

    #[cfg(feature = "bundled")]
    println!("cargo:rustc-link-search=native={}", staging_dir.display());
    for dir in &lib_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }

    let mut cc_build = cc::Build::new();

    if cfg!(feature = "experimental-aec3-config") {
        cc_build.define("WEBRTC_AEC3_CONFIG", None);
    }

    // Set macos minimum version
    if cfg!(target_os = "macos") {
        let min_version = match env::var(MACOSX_DEPLOYMENT_TARGET_VAR) {
            Ok(ver) => ver,
            Err(_) => {
                String::from(match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
                    "x86_64" => "10.10", // Using what I found here https://github.com/webrtc-uwp/chromium-build/blob/master/config/mac/mac_sdk.gni#L17
                    "aarch64" => "11.0", // Apple silicon started here.
                    arch => panic!("unknown arch: {}", arch),
                })
            },
        };

        // `cc` doesn't try to pick up on this automatically, but `clang` needs it to
        // generate a "correct" Objective-C symbol table which better matches XCode.
        // See https://github.com/h4llow3En/mac-notification-sys/issues/45.
        cc_build.flag(format!("-mmacos-version-min={}", min_version));
    }

    // This automatically emits "cargo:rustc-link-lib=static=webrtc_audio_processing_wrapper".
    // The wrapper library should be linked before webrtc-audio-processing-2, otherwise strict
    // linkers (like when passing -Wl,--as-needed) may discard the c++ library (automatically
    // added by cc) from the linking list, resulting in build failure.
    // The linking order should respect the dependency graph, i.e. wrapper -> webrtc-2.
    cc_build
        .cpp(true)
        .file("src/wrapper.cpp")
        .includes(&include_dirs)
        .flag("-std=c++17")
        .flag("-Wno-unused-parameter")
        .out_dir(out_dir());

    // Inform wrapper code that headers for internal classes (ResidualEchoDetector) are available.
    #[cfg(feature = "bundled")]
    cc_build.define("WEBRTC_HAS_INTERNAL_HEADERS", None);

    cc_build.compile("webrtc_audio_processing_wrapper");

    // The the cc and bindgen commands emit `cargo:rerun-if-env-changed=...`, and these deactivate
    // the default behavior to rerun if _any_ source file changes. So state these explicitly.
    // build.rs is always included and doesn't have to be specified.
    println!("cargo:rerun-if-changed=src/wrapper.hpp");
    println!("cargo:rerun-if-changed=src/wrapper.cpp");

    // Prefix the wrapper library's references to webrtc symbols to match the renamed webrtc library.
    let wrapper_lib = out_dir().join(build_support::wrapper_library_filename(
        &target,
        "webrtc_audio_processing_wrapper",
    ));
    if wrapper_lib.exists() {
        prefix_archive_symbols(
            &wrapper_lib,
            &renamed_symbols,
            SYMBOL_PREFIX,
            &target,
            llvm_tools.as_ref(),
        )?;
    } else if build_support::is_msvc_target(&target) && !renamed_symbols.is_empty() {
        bail!(
            "required MSVC wrapper archive is missing after cc build: {}",
            wrapper_lib.display()
        );
    }

    #[cfg(feature = "bundled")]
    {
        println!("{}", build_support::static_link_directive(&bundled_archive));
        for archive in &bundled_abseil_archives {
            println!("{}", build_support::static_link_directive(archive));
        }
    }
    #[cfg(not(feature = "bundled"))]
    {
        println!("cargo:rustc-link-lib=dylib={LIB_NAME}");
    }

    let binding_file = out_dir().join("bindings.rs");
    let mut builder = bindgen::Builder::default()
        .header("src/wrapper.hpp")
        .clang_args(&["-x", "c++", "-std=c++17", "-fparse-all-comments"])
        .generate_comments(true)
        .enable_cxx_namespaces();

    builder = builder
        // Transitive dependencies are automatically included.
        .allowlist_function("webrtc_audio_processing_wrapper::.*")
        .opaque_type("std::.*")
        .parse_callbacks(Box::new(CustomDeriveCallbacks))
        .derive_debug(true)
        // The default implementation ignores C++11's brace-or-equal-initializers,
        // and thus misleading to enable. See also CustomDeriveCallbacks.
        .derive_default(false)
        .derive_partialeq(true);
    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(&binding_file)
        .expect("Couldn't write bindings!");

    Ok(())
}

fn determine_rust_sysroot() -> Result<PathBuf> {
    // 1. Get the rustc command (this might be a path or just "rustc")
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());

    // 2. Ask rustc for the sysroot. This works even if RUSTC="rustc"
    let output = Command::new(&rustc)
        .arg("--print")
        .arg("sysroot")
        .output()
        .context("Failed to execute rustc to find sysroot")?;

    if !output.status.success() {
        bail!("Failed to get sysroot from rustc: {:?}", output);
    }

    let sysroot_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in sysroot")?;
    Ok(PathBuf::from(sysroot_str.trim()))
}

fn determine_rustlib_bin() -> Result<PathBuf> {
    let sysroot = determine_rust_sysroot()?;
    let host = env::var("HOST").context("HOST env var not found")?;
    Ok(sysroot.join("lib").join("rustlib").join(host).join("bin"))
}

fn determine_llvm_tools() -> Result<SymbolTools> {
    let sysroot = determine_rust_sysroot()?;
    let host = env::var("HOST").context("HOST env var not found")?;
    let candidates = build_support::llvm_symbol_tool_candidates(&sysroot, &host);
    let found = candidates
        .iter()
        .filter(|tools| {
            tools.archive.is_file() && tools.nm.is_file() && tools.objcopy.is_file()
        })
        .cloned()
        .collect::<Vec<_>>();
    if found.len() != 1 {
        bail!(
            "required Rust LLVM archive/symbol tools are unavailable for host {host}; searched {candidates:?}; run `rustup component add llvm-tools-preview --toolchain stable`"
        );
    }
    Ok(found.into_iter().next().unwrap())
}

/// Reliably determine a path to objcopy binary bundled with the active Rust toolchain (rust-objcopy)
fn determine_objcopy_path() -> Result<PathBuf> {
    let rustlib_bin = determine_rustlib_bin()?;

    let objcopy = rustlib_bin.join("rust-objcopy");

    // Optional: verification
    if !objcopy.exists() {
        println!("cargo:warning=rust-objcopy not found at {:?}", objcopy);
        println!("cargo:warning=Ensure the 'llvm-tools' component is installed: 'rustup component add llvm-tools'");
    }

    Ok(objcopy)
}
