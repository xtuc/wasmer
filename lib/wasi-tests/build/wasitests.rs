//! This file will run at build time to autogenerate the WASI regression tests
//! It will compile the files indicated in TESTS, to:executable and .wasm
//! - Compile with the native rust target to get the expected output
//! - Compile with the latest WASI target to get the wasm
//! - Generate the test that will compare the output of running the .wasm file
//!   with wasmer with the expected output

use glob::glob;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use std::fs::File;
use std::io::prelude::*;
use std::io::{self, BufReader};

use crate::util;
use crate::wasi_version::*;

static BANNER: &str = "// !!! THIS IS A GENERATED FILE !!!
// ANY MANUAL EDITS MAY BE OVERWRITTEN AT ANY TIME
// Files autogenerated with cargo build (build/wasitests.rs).\n";

/// Compile and execute the test file as native code, saving the results to be
/// compared against later.
///
/// This function attempts to clean up its output after it executes it.
fn generate_native_output(temp_dir: &Path, file: &str, normalized_name: &str) -> io::Result<()> {
    let executable_path = temp_dir.join(normalized_name);
    println!(
        "Compiling program {} to native at {}",
        file,
        executable_path.to_string_lossy()
    );
    let native_out = Command::new("rustc")
        .arg(file)
        .arg("-o")
        .arg(&executable_path)
        .output()
        .expect("Failed to compile program to native code");
    util::print_info_on_error(&native_out, "COMPILATION FAILED");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = executable_path
            .metadata()
            .expect("native executable")
            .permissions();
        perm.set_mode(0o766);
        println!(
            "Setting execute permissions on {}",
            executable_path.to_string_lossy()
        );
        fs::set_permissions(&executable_path, perm)?;
    }

    let result = Command::new(&executable_path)
        .output()
        .expect("Failed to execute native program");
    util::print_info_on_error(&result, "NATIVE PROGRAM FAILED");

    let mut output_path = executable_path.clone();
    output_path.set_extension("out");

    println!("Writing output to {}", output_path.to_string_lossy());
    fs::write(&output_path, result.stdout)?;
    Ok(())
}

/// compile the Wasm file for the given version of WASI
///
/// returns the path of where the wasm file is
fn compile_wasm_for_version(
    temp_dir: &Path,
    file: &str,
    base_dir: &Path,
    rs_mod_name: &str,
    version: WasiVersion,
) -> io::Result<PathBuf> {
    let out_dir = base_dir.join(version.get_directory_name());
    if !out_dir.exists() {
        fs::create_dir(&out_dir)?;
    }
    let wasm_out_name = {
        let mut wasm_out_name = out_dir.join(rs_mod_name);
        wasm_out_name.set_extension("wasm");
        wasm_out_name
    };
    println!("Reading contents from file `{}`", file);
    let file_contents: String = {
        let mut fc = String::new();
        let mut f = fs::OpenOptions::new().read(true).open(&file)?;
        f.read_to_string(&mut fc)?;
        fc
    };

    let temp_wasi_rs_file_name = temp_dir.join(format!("wasi_modified_version_{}.rs", rs_mod_name));
    {
        let mut actual_file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&temp_wasi_rs_file_name)
            .unwrap();
        actual_file.write_all(b"#![feature(wasi_ext)]\n").unwrap();
        actual_file.write_all(file_contents.as_bytes()).unwrap();
    }

    println!(
        "Compiling wasm module `{}` with toolchain `{}`",
        &wasm_out_name.to_string_lossy(),
        version.get_compiler_toolchain()
    );
    let wasm_compilation_out = Command::new("rustc")
        .arg(format!("+{}", version.get_compiler_toolchain()))
        .arg("--target=wasm32-wasi")
        .arg("-C")
        .arg("opt-level=z")
        .arg(&temp_wasi_rs_file_name)
        .arg("-o")
        .arg(&wasm_out_name)
        .output()
        .expect("Failed to compile program to wasm");
    util::print_info_on_error(&wasm_compilation_out, "WASM COMPILATION");
    println!(
        "Removing file `{}`",
        &temp_wasi_rs_file_name.to_string_lossy()
    );

    // to prevent commiting huge binary blobs forever
    let wasm_strip_out = Command::new("wasm-strip")
        .arg(&wasm_out_name)
        .output()
        .expect("Failed to strip compiled wasm module");
    util::print_info_on_error(&wasm_strip_out, "STRIPPING WASM");
    let wasm_opt_out = Command::new("wasm-opt")
        .arg("-Oz")
        .arg(&wasm_out_name)
        .arg("-o")
        .arg(&wasm_out_name)
        .output()
        .expect("Failed to optimize compiled wasm module with wasm-opt!");
    util::print_info_on_error(&wasm_opt_out, "OPTIMIZING WASM");

    Ok(wasm_out_name)
}

fn generate_test_file(
    file: &str,
    rs_module_name: &str,
    wasm_out_name: &str,
    version: WasiVersion,
    ignores: &HashSet<String>,
) -> io::Result<String> {
    let test_name = format!("{}_{}", version.get_directory_name(), rs_module_name);
    let ignored = if ignores.contains(&test_name) || ignores.contains(rs_module_name) {
        "\n#[ignore]"
    } else {
        ""
    };

    let src_code = fs::read_to_string(file)?;
    let args: Args = extract_args_from_source_file(&src_code).unwrap_or_default();

    let mapdir_args = {
        let mut out_str = String::new();
        out_str.push_str("vec![");
        for (alias, real_dir) in args.mapdir {
            out_str.push_str(&format!(
                "(\"{}\".to_string(), ::std::path::PathBuf::from(\"{}\")),",
                alias, real_dir
            ));
        }
        out_str.push_str("]");
        out_str
    };

    let envvar_args = {
        let mut out_str = String::new();
        out_str.push_str("vec![");

        for entry in args.envvars {
            out_str.push_str(&format!("\"{}={}\".to_string(),", entry.0, entry.1));
        }

        out_str.push_str("]");
        out_str
    };

    let dir_args = {
        let mut out_str = String::new();
        out_str.push_str("vec![");

        for entry in args.po_dirs {
            out_str.push_str(&format!("std::path::PathBuf::from(\"{}\"),", entry));
        }

        out_str.push_str("]");
        out_str
    };

    let contents = format!(
        "{banner}

#[test]{ignore}
fn test_{test_name}() {{
    assert_wasi_output!(
        \"../../{module_path}\",
        \"{test_name}\",
        {dir_args},
        {mapdir_args},
        {envvar_args},
        \"../../{test_output_path}\"
    );
}}
",
        banner = BANNER,
        ignore = ignored,
        module_path = wasm_out_name,
        test_name = &test_name,
        test_output_path = format!("wasitests/{}.out", rs_module_name),
        dir_args = dir_args,
        mapdir_args = mapdir_args,
        envvar_args = envvar_args
    );
    let rust_test_filepath = format!(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wasitests/{}.rs"),
        &test_name,
    );
    fs::write(&rust_test_filepath, contents.as_bytes())?;

    Ok(test_name)
}

/// Returns the a Vec of the test modules created
fn compile(
    temp_dir: &Path,
    file: &str,
    ignores: &HashSet<String>,
    wasi_versions: &[WasiVersion],
) -> Vec<String> {
    // TODO: hook up compile_wasm_for_version, etc with new args
    assert!(file.ends_with(".rs"));
    let rs_mod_name = {
        Path::new(&file.to_lowercase())
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string()
    };
    let base_dir = Path::new(file).parent().unwrap();
    generate_native_output(temp_dir, &file, &rs_mod_name).expect("Generate native output");
    let mut out = vec![];

    for &version in wasi_versions {
        let wasm_out_path = compile_wasm_for_version(temp_dir, file, base_dir, &rs_mod_name, version)
            .expect(&format!("Could not compile Wasm to WASI version {:?}, perhaps you need to install the `{}` rust toolchain", version, version.get_compiler_toolchain()));
        let wasm_out_name = wasm_out_path.to_string_lossy();
        let test_mod = generate_test_file(file, &rs_mod_name, &wasm_out_name, version, ignores)
            .expect(&format!("generate test file {}", &rs_mod_name));
        out.push(test_mod);
    }

    out
}

fn run_prelude(should_gen_all: bool) -> &'static [WasiVersion] {
    if should_gen_all {
        println!(
            "Generating WASI tests for all versions of WASI. Run with WASI_TEST_GENERATE_ALL=0 to only generate the latest tests."
        );
    } else {
        println!(
            "Generating WASI tests for the latest version of WASI. Run with WASI_TEST_GENERATE_ALL=1 to generate all versions of the tests."
        );
    }

    if should_gen_all {
        ALL_WASI_VERSIONS
    } else {
        LATEST_WASI_VERSION
    }
}

pub fn build(should_gen_all: bool) {
    let rust_test_modpath = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wasitests/mod.rs");

    let mut modules: Vec<String> = Vec::new();
    let wasi_versions = run_prelude(should_gen_all);

    let temp_dir = tempfile::TempDir::new().unwrap();
    let ignores = read_ignore_list();
    for entry in glob("wasitests/*.rs").unwrap() {
        match entry {
            Ok(path) => {
                let test = path.to_str().unwrap();
                modules.extend(compile(temp_dir.path(), test, &ignores, wasi_versions));
            }
            Err(e) => println!("{:?}", e),
        }
    }
    println!("All modules generated. Generating test harness.");
    modules.sort();
    let mut modules: Vec<String> = modules.iter().map(|m| format!("mod {};", m)).collect();
    assert!(modules.len() > 0, "Expected > 0 modules found");

    modules.insert(0, BANNER.to_string());
    modules.insert(1, "// The _common module is not autogenerated.  It provides common macros for the wasitests\n#[macro_use]\nmod _common;".to_string());
    // We add an empty line
    modules.push("".to_string());

    let modfile: String = modules.join("\n");
    let should_regen: bool = {
        if let Ok(mut f) = fs::File::open(&rust_test_modpath) {
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            s != modfile
        } else {
            false
        }
    };
    if should_regen {
        println!("Writing to `{}`", &rust_test_modpath);
        let mut test_harness_file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&rust_test_modpath)
            .unwrap();
        test_harness_file.write_all(modfile.as_bytes()).unwrap();
    }
}

fn read_ignore_list() -> HashSet<String> {
    let f = File::open("wasitests/ignores.txt").unwrap();
    let f = BufReader::new(f);
    f.lines()
        .filter_map(Result::ok)
        .map(|v| v.to_lowercase())
        .collect()
}

#[derive(Debug, Default)]
struct Args {
    pub mapdir: Vec<(String, String)>,
    pub envvars: Vec<(String, String)>,
    /// pre-opened directories
    pub po_dirs: Vec<String>,
}

/// Pulls args to the program out of a comment at the top of the file starting with "// Args:"
fn extract_args_from_source_file(source_code: &str) -> Option<Args> {
    if source_code.starts_with("// Args:") {
        let mut args = Args::default();
        for arg_line in source_code
            .lines()
            .skip(1)
            .take_while(|line| line.starts_with("// "))
        {
            let tokenized = arg_line
                .split_whitespace()
                // skip trailing space
                .skip(1)
                .map(String::from)
                .collect::<Vec<String>>();
            let command_name = {
                let mut cn = tokenized[0].clone();
                assert_eq!(
                    cn.pop(),
                    Some(':'),
                    "Final character of argname must be a colon"
                );
                cn
            };

            match command_name.as_ref() {
                "mapdir" => {
                    if let [alias, real_dir] = &tokenized[1].split(':').collect::<Vec<&str>>()[..] {
                        args.mapdir.push((alias.to_string(), real_dir.to_string()));
                    } else {
                        eprintln!(
                            "Parse error in mapdir {} not parsed correctly",
                            &tokenized[1]
                        );
                    }
                }
                "env" => {
                    if let [name, val] = &tokenized[1].split('=').collect::<Vec<&str>>()[..] {
                        args.envvars.push((name.to_string(), val.to_string()));
                    } else {
                        eprintln!("Parse error in env {} not parsed correctly", &tokenized[1]);
                    }
                }
                "dir" => {
                    args.po_dirs.push(tokenized[1].to_string());
                }
                e => {
                    eprintln!("WARN: comment arg: {} is not supported", e);
                }
            }
        }
        return Some(args);
    }
    None
}
