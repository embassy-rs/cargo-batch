#![warn(rust_2018_idioms)] // while we're getting used to 2018
#![allow(clippy::all)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::redundant_clone)]

use cargo_batch as cargo;

use cargo::core::compiler::{
    unit_graph, BuildContext, Context, DefaultExecutor, Executor, UnitInterner,
};
use cargo::core::shell::Shell;
use cargo::core::Workspace;
use cargo::ops::CompileOptions;
use cargo::util::command_prelude::App;
use cargo::util::{ command_prelude,  CliResult, Config};
use cargo::util::{profile};
use std::env;
use std::sync::Arc;

use crate::command_prelude::*;

fn main() {
    #[cfg(feature = "pretty-env-logger")]
    pretty_env_logger::init_custom_env("CARGO_LOG");
    #[cfg(not(feature = "pretty-env-logger"))]
    env_logger::init_from_env("CARGO_LOG");

    let mut config = match Config::default() {
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

fn main2(config: &mut Config) -> CliResult {
    let args: Vec<_> = env::args().collect();
    let mut subargs = args.split(|x| *x == "---");

    let global_args = subargs.next().unwrap();
    let global_args = App::new("cargo-batch")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::AllowExternalSubcommands)
        .setting(AppSettings::NoAutoVersion)
        .arg(opt("unit-graph", "Output build graph in JSON (unstable)"))
        .try_get_matches_from(global_args)?;

    config_configure(config, &global_args)?;
    init_git_transports(config);

    let unit_graph = global_args._is_present("unit-graph");

    struct Command<'a> {
        ws: Workspace<'a>,
        compile_opts: CompileOptions,
    }

    let mut cmds = Vec::new();
    for args in subargs {
        let cli = build_cli();
        let args = cli.try_get_matches_from(args)?;
        //println!("args opts: {:#?}", args);

        let ws = args.workspace(config)?;

        let mut compile_opts = args.compile_options(
            config,
            CompileMode::Build,
            Some(&ws),
            ProfileChecking::Custom,
        )?;
        if let Some(out_dir) = args.value_of_path("out-dir", config) {
            compile_opts.build_config.export_dir = Some(out_dir);
        } else if let Some(out_dir) = config.build_config()?.out_dir.as_ref() {
            let out_dir = out_dir.resolve_path(config);
            compile_opts.build_config.export_dir = Some(out_dir);
        }
        //if compile_opts.build_config.export_dir.is_some() {
        //    config
        //        .cli_unstable()
        //        .fail_if_stable_opt("--out-dir", 6790)?;
        //}

        //println!("compile opts: {:#?}", compile_opts);
        cmds.push(Command { ws, compile_opts });
    }

    let interner = UnitInterner::new();
    let mut merged_bcx: Option<BuildContext<'_, '_>> = None;

    for cmd in &cmds {
        let mut bcx = cargo::ops::create_bcx(&cmd.ws, &cmd.compile_opts, &interner).unwrap();
        if let Some(export_dir) = &cmd.compile_opts.build_config.export_dir {
            for root in &bcx.roots {
                bcx.unit_export_dirs
                    .insert(root.clone(), export_dir.clone());
            }
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
        } else {
            merged_bcx = Some(bcx)
        }
    }

    let bcx = merged_bcx.unwrap();

    if unit_graph {
        unit_graph::emit_serialized_unit_graph(&bcx.roots, &bcx.unit_graph, bcx.ws.config())?;
        return Ok(());
    }

    let _p = profile::start("compiling");
    let cx = Context::new(&bcx)?;
    let exec: Arc<dyn Executor> = Arc::new(DefaultExecutor);
    cx.compile(&exec)?;

    Ok(())
}

fn config_configure(config: &mut Config, args: &ArgMatches) -> CliResult {
    let arg_target_dir = &args.value_of_path("target-dir", config);
    let verbose = args.occurrences_of("verbose") as u32;
    // quiet is unusual because it is redefined in some subcommands in order
    // to provide custom help text.
    let quiet = args.is_present("quiet");
    let color = args.value_of("color");
    let frozen = args.is_present("frozen");
    let locked = args.is_present("locked");
    let offline = args.is_present("offline");

    let unstable_flags = args
        .values_of_lossy("unstable-features")
        .unwrap_or_default();

    let config_args: Vec<_> = args
        .values_of("config")
        .unwrap_or_default()
        .map(|s| s.to_string())
        .collect();

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

fn init_git_transports(config: &Config) {
    // Only use a custom transport if any HTTP options are specified,
    // such as proxies or custom certificate authorities. The custom
    // transport, however, is not as well battle-tested.

    match cargo::ops::needs_custom_http_transport(config) {
        Ok(true) => {}
        _ => return,
    }

    let handle = match cargo::ops::http_handle(config) {
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

pub fn build_cli() -> App {
    subcommand("build")
        // subcommand aliases are handled in aliased_command()
        // .alias("b")
        .about("Compile a local package and all of its dependencies")
        .arg(opt("quiet", "No output printed to stdout").short('q'))
        .arg_package_spec(
            "Package to build (see `cargo help pkgid`)",
            "Build all packages in the workspace",
            "Exclude packages from the build",
        )
        .arg_jobs()
        .arg_targets_all(
            "Build only this package's library",
            "Build only the specified binary",
            "Build all binaries",
            "Build only the specified example",
            "Build all examples",
            "Build only the specified test target",
            "Build all tests",
            "Build only the specified bench target",
            "Build all benches",
            "Build all targets",
        )
        .arg_release("Build artifacts in release mode, with optimizations")
        .arg_profile("Build artifacts with the specified profile")
        .arg_features()
        .arg_target_triple("Build for the target triple")
        .arg_target_dir()
        .arg(
            opt(
                "out-dir",
                "Copy final artifacts to this directory (unstable)",
            )
            .value_name("PATH"),
        )
        .arg_manifest_path()
        .arg_ignore_rust_version()
        .arg_message_format()
        .arg_build_plan()
        .arg_unit_graph()
        .arg_future_incompat_report()
        .after_help("Run `cargo help build` for more detailed information.\n")
}
