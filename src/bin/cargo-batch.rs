#![warn(rust_2018_idioms)] // while we're getting used to 2018
#![allow(clippy::all)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::redundant_clone)]

use cargo::core::compiler::{
    unit_graph, BuildContext, BuildRunner, DefaultExecutor, Executor, Unit, UnitIndex, UnitInterner,
};
use cargo::core::Workspace;
use cargo_util_terminal::Shell;
use cargo::ops::{CompileOptions, OutputFormat};
use cargo::util::network::http::{http_handle, needs_custom_http_transport};
use cargo::util::{command_prelude, CliResult, GlobalContext};
use std::collections::HashSet;
use std::sync::Arc;
use std::{env, io};

use crate::command_prelude::*;

fn main() {
    setup_logger();

    let mut config = match GlobalContext::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };

    let result = main2(&mut config);

    match result {
        Err(e) => cargo::exit_with_error(e, &mut *config.shell()),
        Ok(()) => {}
    }
}

fn main2(gctx: &mut GlobalContext) -> CliResult {
    let args: Vec<_> = env::args().collect();
    let mut subargs = args.split(|x| *x == "---");

    let mut global_args = subargs.next().unwrap().to_vec();
    if global_args.len() >= 2 && global_args[1] == "batch" {
        global_args.remove(1);
    }
    let global_args = Command::new("cargo-batch")
        .arg_unit_graph()
        .arg_target_dir()
        .arg(
            opt(
                "verbose",
                "Use verbose output (-vv very verbose/build.rs output)",
            )
            .short('v')
            .action(ArgAction::Count)
            .global(true),
        )
        .arg_silent_suggestion()
        .arg(flag("stdin", "Collect arguments from stdin").global(true))
        .arg(
            opt("color", "Coloring: auto, always, never")
                .value_name("WHEN")
                .global(true),
        )
        .arg(
            flag("frozen", "Require Cargo.lock and cache are up to date")
                .help_heading(heading::MANIFEST_OPTIONS)
                .global(true),
        )
        .arg(
            flag("locked", "Require Cargo.lock is up to date")
                .help_heading(heading::MANIFEST_OPTIONS)
                .global(true),
        )
        .arg(
            flag("offline", "Run without accessing the network")
                .help_heading(heading::MANIFEST_OPTIONS)
                .global(true),
        )
        .arg(multi_opt("config", "KEY=VALUE", "Override a configuration value").global(true))
        .arg(
            Arg::new("unstable-features")
                .help("Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for details")
                .short('Z')
                .value_name("FLAG")
                .action(ArgAction::Append)
                .global(true),
        )
        .try_get_matches_from(global_args)?;

    config_configure(gctx, &global_args)?;
    init_git_transports(gctx);

    let unit_graph = global_args.flag("unit-graph");

    /// What a `test` command wants done once its units are built.
    struct TestState {
        no_run: bool,
        no_fail_fast: bool,
        args: Vec<String>,
        /// Root units of this command, filled in when its build context is
        /// created. Everything is compiled as one merged build, so this is what
        /// tells the tests of this command apart from everyone else's.
        roots: Vec<Unit>,
    }

    struct CommandState {
        /// Index into `workspaces`.
        ws: usize,
        compile_opts: CompileOptions,
        /// `Some` if this is a `test` command.
        test: Option<TestState>,
    }

    // Loading a workspace parses its manifest and every member's manifest, which
    // is expensive in both time and memory. A batch typically contains many
    // commands for the same manifest (one per feature combination), so load each
    // one once and share it. The only per-command inputs to `args.workspace()`
    // are the root manifest path and `--ignore-rust-version`, so key on those.
    let mut workspaces: Vec<Workspace<'_>> = Vec::new();
    let mut workspace_index: std::collections::HashMap<(std::path::PathBuf, Option<bool>), usize> =
        Default::default();

    let subargs: Vec<Vec<String>> = if global_args.get_flag("stdin") {
        io::stdin()
            .lines()
            .filter_map(|x| x.ok())
            .map(|l| shell_words::split(&l).unwrap())
            .collect()
    } else {
        subargs
            .map(|x| x.iter().map(|x| x.clone()).collect::<Vec<_>>())
            .collect()
    };

    let mut cmds = Vec::new();
    for args in subargs {
        let cli = build_cli();
        let args = cli.try_get_matches_from([String::new()].into_iter().chain(args.into_iter()))?;
        let (subcmd, args) = args.subcommand().unwrap();

        let ws_key = (args.root_manifest(gctx)?, args.honor_rust_version());
        let ws_idx = match workspace_index.get(&ws_key) {
            Some(&idx) => idx,
            None => {
                workspaces.push(args.workspace(gctx)?);
                let idx = workspaces.len() - 1;
                workspace_index.insert(ws_key, idx);
                idx
            }
        };
        let ws = &workspaces[ws_idx];

        match subcmd {
            "build" => {
                let mut compile_opts = args.compile_options(
                    gctx,
                    UserIntent::Build,
                    Some(ws),
                    ProfileChecking::Custom,
                )?;
                if let Some(out_dir) = args.value_of_path("artifact-dir", gctx) {
                    compile_opts.build_config.export_dir = Some(out_dir);
                }

                //println!("compile opts: {:#?}", compile_opts);
                cmds.push(CommandState {
                    ws: ws_idx,
                    compile_opts,
                    test: None,
                });
            }
            "test" => {
                let mut compile_opts = args.compile_options(
                    gctx,
                    UserIntent::Test,
                    Some(ws),
                    ProfileChecking::Custom,
                )?;
                compile_opts.build_config.requested_profile =
                    args.get_profile_name("test", ProfileChecking::Custom)?;

                // `TESTNAME` is actually an argument of the test binary, but
                // it's important, so it gets a name of its own.
                let test_name = args.get_one::<String>("TESTNAME");
                let test_args = test_name
                    .into_iter()
                    .chain(args.get_many::<String>("args").unwrap_or_default())
                    .cloned()
                    .collect::<Vec<_>>();

                let no_run = args.flag("no-run");
                if args.flag("doc") {
                    if compile_opts.filter.is_specific() {
                        return Err(anyhow::format_err!(
                            "can't mix --doc with other target selecting options"
                        )
                        .into());
                    }
                    if no_run {
                        return Err(anyhow::format_err!(
                            "can't skip running doc tests with --no-run"
                        )
                        .into());
                    }
                    compile_opts.build_config.intent = UserIntent::Doctest;
                    compile_opts.filter = cargo::ops::CompileFilter::lib_only();
                } else if test_name.is_some() && !compile_opts.filter.is_specific() {
                    // If `TESTNAME` is given, the user knows what they want to
                    // run, so don't build targets the default filter would pull
                    // in (examples, most notably).
                    compile_opts.filter = cargo::ops::CompileFilter::all_test_targets();
                }

                cmds.push(CommandState {
                    ws: ws_idx,
                    compile_opts,
                    test: Some(TestState {
                        no_run,
                        no_fail_fast: args.flag("no-fail-fast"),
                        args: test_args,
                        roots: Vec::new(),
                    }),
                });
            }
            "check" => {
                // This is a legacy behavior that causes `cargo check` to pass `--test`.
                let test = matches!(
                    args.get_one::<String>("profile").map(String::as_str),
                    Some("test")
                );
                let mode = UserIntent::Check { test };
                let compile_opts =
                    args.compile_options(gctx, mode, Some(ws), ProfileChecking::LegacyTestOnly)?;

                cmds.push(CommandState {
                    ws: ws_idx,
                    compile_opts,
                    test: None,
                });
            }
            "rustdoc" => {
                let output_format = if let Some(output_format) = args._value_of("output-format") {
                    gctx.cli_unstable()
                        .fail_if_stable_opt("--output-format", 12103)?;
                    output_format.parse()?
                } else {
                    OutputFormat::Html
                };

                //panic!("output-format {output_format:?}");

                let mut compile_opts = args.compile_options(
                    gctx,
                    UserIntent::Doc {
                        deps: false,
                        json: matches!(output_format, OutputFormat::Json),
                    },
                    Some(ws),
                    ProfileChecking::Custom,
                )?;
                if let Some(out_dir) = args.value_of_path("artifact-dir", gctx) {
                    compile_opts.build_config.export_dir = Some(out_dir);
                }
                let target_args = values(args, "args");
                compile_opts.target_rustdoc_args = if target_args.is_empty() {
                    None
                } else {
                    Some(target_args)
                };

                //println!("compile opts: {:#?}", compile_opts);
                cmds.push(CommandState {
                    ws: ws_idx,
                    compile_opts,
                    test: None,
                });
            }
            _ => unreachable!(),
        }
    }

    let interner = UnitInterner::new();
    let mut merged_bcx: Option<BuildContext<'_, '_>> = None;

    for cmd in &mut cmds {
        let export_dir = cmd.compile_opts.build_config.export_dir.take();

        let mut bcx =
            cargo::ops::create_bcx(&workspaces[cmd.ws], &cmd.compile_opts, &interner, None)
                .unwrap();
        if let Some(export_dir) = export_dir {
            for root in &bcx.roots {
                bcx.unit_export_dirs
                    .insert(root.clone(), export_dir.clone());
            }
        }

        if let Some(test) = &mut cmd.test {
            test.roots = bcx.roots.clone();
        }

        if let Some(merged_bcx) = &mut merged_bcx {
            // merge!!!
            merged_bcx.unit_graph.extend(bcx.unit_graph);
            merged_bcx.roots.extend(bcx.roots);
            merged_bcx.unit_export_dirs.extend(bcx.unit_export_dirs);
            merged_bcx.all_kinds.extend(bcx.all_kinds);
            merged_bcx
                .target_data
                .target_config
                .extend(bcx.target_data.target_config);
            merged_bcx
                .target_data
                .target_info
                .extend(bcx.target_data.target_info);
            merged_bcx.packages.packages.extend(bcx.packages.packages);
            merged_bcx
                .packages
                .sources
                .borrow_mut()
                .add_source_map(bcx.packages.sources.into_inner());
            merged_bcx
                .extra_compiler_args
                .extend(bcx.extra_compiler_args);
            merged_bcx.selected_dep_kinds.build |= bcx.selected_dep_kinds.build;
            merged_bcx.selected_dep_kinds.normal |= bcx.selected_dep_kinds.normal;
            merged_bcx.selected_dep_kinds.dev |= bcx.selected_dep_kinds.dev;
        } else {
            merged_bcx = Some(bcx)
        }
    }

    let mut bcx = merged_bcx.unwrap();
    bcx.do_uplift = false;

    // Two commands can ask for the same unit (the same features, or one command
    // that is a subset of another), and merging their roots then lists it twice.
    // `compile` collects outputs per root, so a duplicate root means a duplicate
    // entry in `compilation.tests`, i.e. a test binary run twice.
    let mut seen = HashSet::new();
    bcx.roots.retain(|unit| seen.insert(unit.clone()));

    // `create_bcx` assigns unit indexes per build context, so after merging several
    // of them the indexes are neither unique nor complete. Reassign them over the
    // merged unit graph, the same way `create_bcx` does.
    let mut units: Vec<_> = bcx.unit_graph.keys().cloned().collect();
    units.sort();
    bcx.unit_to_index = units
        .into_iter()
        .enumerate()
        .map(|(i, unit)| (unit, UnitIndex(i as u64)))
        .collect();

    if unit_graph {
        unit_graph::emit_serialized_unit_graph(&bcx.roots, &bcx.unit_graph, bcx.ws.gctx())?;
        return Ok(());
    }

    // util::profile disappeared between cargo 1.76 and cargo 1.78
    // let _p = cargo::util::profile::start("compiling");
    let cx = BuildRunner::new(&bcx)?;
    let exec: Arc<dyn Executor> = Arc::new(DefaultExecutor);
    let compilation = cx.compile(&exec)?;

    // Everything is built now. Run the tests of each `test` command, in the
    // order the commands were given, over the units that command asked for.
    for cmd in &cmds {
        let Some(test) = &cmd.test else { continue };

        let roots: HashSet<&Unit> = test.roots.iter().collect();
        let test_args: Vec<&str> = test.args.iter().map(String::as_str).collect();

        cargo::ops::run_compiled_tests(
            &workspaces[cmd.ws],
            &cmd.compile_opts,
            test.no_run,
            test.no_fail_fast,
            &test_args,
            &compilation,
            &|unit| roots.contains(unit),
        )?;
    }

    Ok(())
}

fn config_configure(config: &mut GlobalContext, args: &ArgMatches) -> CliResult {
    let arg_target_dir = &args.value_of_path("target-dir", config);
    let verbose = args.verbose();
    // quiet is unusual because it is redefined in some subcommands in order
    // to provide custom help text.
    let quiet = args.flag("quiet");
    let color = args.get_one::<String>("color").map(String::as_str);
    let frozen = args.flag("frozen");
    let locked = args.flag("locked");
    let offline = args.flag("offline");
    let mut unstable_flags = vec![];
    if let Some(values) = args.get_many::<String>("unstable-features") {
        unstable_flags.extend(values.cloned());
    }
    let mut config_args = vec![];
    if let Some(values) = args.get_many::<String>("config") {
        config_args.extend(values.cloned());
    }
    config.configure(
        verbose,
        quiet,
        color,
        frozen,
        locked,
        offline,
        arg_target_dir,
        &unstable_flags,
        &config_args,
    )?;
    Ok(())
}

pub fn build_cli() -> Command {
    subcommand("cargo-batch")
        .subcommand(
            subcommand("build")
                .about("Compile a local package and all of its dependencies")
                .arg_future_incompat_report()
                .arg_package_spec(
                    "Package to build (see `cargo help pkgid`)",
                    "Build all packages in the workspace",
                    "Exclude packages from the build",
                )
                .arg_targets_all(
                    "Build only this package's library",
                    "Build only the specified binary",
                    "Build all binaries",
                    "Build only the specified example",
                    "Build all examples",
                    "Build only the specified test target",
                    "Build all targets that have `test = true` set",
                    "Build only the specified bench target",
                    "Build all targets that have `bench = true` set",
                    "Build all targets",
                )
                .arg_features()
                .arg_release("Build artifacts in release mode, with optimizations")
                .arg_redundant_default_mode("debug", "build", "release")
                .arg_profile("Build artifacts with the specified profile")
                .arg_target_triple("Build for the target triple")
                .arg_artifact_dir()
                .arg_manifest_path()
                .arg_ignore_rust_version()
                .after_help(color_print::cstr!(
                    "Run `<cyan,bold>cargo help build</>` for more detailed information.\n"
                )),
        )
        .subcommand(
            subcommand("rustdoc")
                .about("Build a package's documentation, using specified custom flags.")
                .arg(
                    Arg::new("args")
                        .value_name("ARGS")
                        .help("Extra rustdoc flags")
                        .num_args(0..)
                        .trailing_var_arg(true),
                )
                .arg_package("Package to document")
                .arg_targets_all(
                    "Build only this package's library",
                    "Build only the specified binary",
                    "Build all binaries",
                    "Build only the specified example",
                    "Build all examples",
                    "Build only the specified test target",
                    "Build all targets that have `test = true` set",
                    "Build only the specified bench target",
                    "Build all targets that have `bench = true` set",
                    "Build all targets",
                )
                .arg_features()
                .arg_release("Build artifacts in release mode, with optimizations")
                .arg_profile("Build artifacts with the specified profile")
                .arg_target_triple("Build for the target triple")
                .arg(
                    opt("output-format", "The output type to write (unstable)")
                        .value_name("FMT")
                        .value_parser(OutputFormat::POSSIBLE_VALUES),
                )
                .arg_artifact_dir()
                .arg_manifest_path()
                .arg_ignore_rust_version()
                .after_help(color_print::cstr!(
                    "Run `<cyan,bold>cargo help rustdoc</>` for more detailed information.\n"
                )),
        )
        .subcommand(
            subcommand("test")
                // subcommand aliases are handled in aliased_command()
                // .alias("t")
                .about("Execute all unit and integration tests and build examples of a local package")
                .arg(
                    Arg::new("TESTNAME")
                        .action(ArgAction::Set)
                        .help("If specified, only run tests containing this string in their names"),
                )
                .arg(
                    Arg::new("args")
                        .value_name("ARGS")
                        .help("Arguments for the test binary")
                        .num_args(0..)
                        .last(true),
                )
                .arg(flag("no-run", "Compile, but don't run tests"))
                .arg(flag("no-fail-fast", "Run all tests regardless of failure"))
                .arg_future_incompat_report()
                .arg_message_format()
                .arg(
                    flag(
                        "quiet",
                        "Display one character per test instead of one line",
                    )
                    .short('q'),
                )
                .arg_package_spec(
                    "Package to run tests for",
                    "Test all packages in the workspace",
                    "Exclude packages from the test",
                )
                .arg_targets_all(
                    "Test only this package's library",
                    "Test only the specified binary",
                    "Test all binaries",
                    "Test only the specified example",
                    "Test all examples",
                    "Test only the specified test target",
                    "Test all targets that have `test = true` set",
                    "Test only the specified bench target",
                    "Test all targets that have `bench = true` set",
                    "Test all targets (does not include doctests)",
                )
                .arg(
                    flag("doc", "Test only this library's documentation")
                        .help_heading(heading::TARGET_SELECTION),
                )
                .arg_features()
                .arg_release("Build artifacts in release mode, with optimizations")
                .arg_profile("Build artifacts with the specified profile")
                .arg_target_triple("Build for the target triple")
                .arg_manifest_path()
                .arg_ignore_rust_version()
                .after_help(color_print::cstr!(
                    "Run `<cyan,bold>cargo help test</>` for more detailed information.\n\
                     Run `<cyan,bold>cargo test -- --help</>` for test binary options.\n",
                )),
        )
        .subcommand(
            subcommand("check")
                // subcommand aliases are handled in aliased_command()
                // .alias("c")
                .about("Check a local package and all of its dependencies for errors")
                .arg_future_incompat_report()
                .arg_message_format()
                .arg_silent_suggestion()
                .arg_package_spec(
                    "Package(s) to check",
                    "Check all packages in the workspace",
                    "Exclude packages from the check",
                )
                .arg_targets_all(
                    "Check only this package's library",
                    "Check only the specified binary",
                    "Check all binaries",
                    "Check only the specified example",
                    "Check all examples",
                    "Check only the specified test target",
                    "Check all targets that have `test = true` set",
                    "Check only the specified bench target",
                    "Check all targets that have `bench = true` set",
                    "Check all targets",
                )
                .arg_features()
                .arg_parallel()
                .arg_release("Check artifacts in release mode, with optimizations")
                .arg_profile("Check artifacts with the specified profile")
                .arg_target_triple("Check for the target triple")
                .arg_target_dir()
                .arg_unit_graph()
                .arg_timings()
                .arg_manifest_path()
                .arg_ignore_rust_version()
                .after_help(color_print::cstr!(
                    "Run `<cyan,bold>cargo help check</>` for more detailed information.\n"
                )),
        )
}

fn setup_logger() {
    let env = tracing_subscriber::EnvFilter::from_env("CARGO_LOG");

    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::Uptime::default())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(env)
        .init();
    tracing::trace!(start = jiff::Timestamp::now().to_string());
}

/// Configure libgit2 to use libcurl if necessary.
///
/// If the user has a non-default network configuration, then libgit2 will be
/// configured to use libcurl instead of the built-in networking support so
/// that those configuration settings can be used.
fn init_git_transports(config: &GlobalContext) {
    match needs_custom_http_transport(config) {
        Ok(true) => {}
        _ => return,
    }

    let handle = match http_handle(config) {
        Ok(handle) => handle,
        Err(..) => return,
    };

    // The unsafety of the registration function derives from two aspects:
    //
    // 1. This call must be synchronized with all other registration calls as
    //    well as construction of new transports.
    // 2. The argument is leaked.
    //
    // We're clear on point (1) because this is only called at the start of this
    // binary (we know what the state of the world looks like) and we're mostly
    // clear on point (2) because we'd only free it after everything is done
    // anyway
    unsafe {
        git2_curl::register(handle);
    }
}
