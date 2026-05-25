use std::collections::HashMap;

use cargo_rapk::{ApkBuilder, Error};
use cargo_subcommand::Subcommand;
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Parser)]
struct Cmd {
    #[clap(subcommand)]
    apk: RapkCmd,
}

#[derive(clap::Subcommand)]
enum RapkCmd {
    /// Helps cargo build apks for Android
    Rapk {
        #[clap(subcommand)]
        cmd: RapkSubCmd,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Parser)]
#[group(skip)]
struct Args {
    #[clap(flatten)]
    subcommand_args: cargo_subcommand::Args,
    /// Use device with the given serial (see `adb devices`)
    #[clap(short, long)]
    device: Option<String>,

    // Reproducibility knobs
    /// Enable deterministic (reproducible) build settings
    #[clap(long, env = "CARGO_RAPK_DETERMINISTIC")]
    deterministic: bool,
    /// Produce an unsigned APK
    #[clap(long, env = "CARGO_RAPK_NO_SIGN")]
    unsigned: bool,
    /// zipalign alignment in bytes (default: 4)
    #[clap(long = "align", env = "CARGO_RAPK_ALIGN", default_value = "4")]
    align: u32,
    /// UNIX timestamp used for ZIP mtimes (defaults to SOURCE_DATE_EPOCH if present)
    #[clap(long = "timestamp")]
    timestamp: Option<u64>,
    /// Disable ZIP normalization (escape hatch)
    #[clap(long = "no-normalize-zip")]
    no_normalize_zip: bool,
}

#[derive(clap::Subcommand)]
enum RapkSubCmd {
    /// Analyze the current package and report errors, but don't build object files nor an apk
    #[clap(visible_alias = "c")]
    Check {
        #[clap(flatten)]
        args: Args,
    },
    /// Compile the current package and create an apk
    #[clap(visible_alias = "b")]
    Build {
        #[clap(flatten)]
        args: Args,
    },
    /// Invoke `cargo` under the detected NDK environment
    #[clap(name = "--")]
    Ndk {
        /// `cargo` subcommand to run
        cargo_cmd: String,

        /// Arguments passed to cargo. Some arguments will be used to configure
        /// the environment similar to other `cargo rapk` commands
        // TODO: This enum variant should parse into `Args` as soon as `clap` supports
        // parsing only unrecognized args into a side-buffer.
        #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
        cargo_args: Vec<String>,
    },
    /// Run a binary or example apk of the local package
    #[clap(visible_alias = "r")]
    Run {
        #[clap(flatten)]
        args: Args,
        /// Do not print or follow `logcat` after running the app
        #[clap(short, long)]
        no_logcat: bool,
    },
    /// Start a gdb session attached to an adb device with symbols loaded
    Gdb {
        #[clap(flatten)]
        args: Args,
    },
    /// Print the version of cargo-rapk
    Version,
}

fn split_apk_and_cargo_args(input: Vec<String>) -> anyhow::Result<(Args, Vec<String>)> {
    // Clap doesn't support parsing unknown args properly:
    // https://github.com/clap-rs/clap/issues/1404
    // https://github.com/clap-rs/clap/issues/4498
    // Introspect the `Args` struct and extract every known arg, and whether it takes a value. Use
    // this information to separate out known args from unknown args, and re-parse all the known
    // args into an `Args` struct.

    let known_args_taking_value = Args::command()
        .get_arguments()
        .flat_map(|arg| {
            assert!(!arg.is_positional());
            arg.get_short_and_visible_aliases()
                .iter()
                .flat_map(|shorts| shorts.iter().map(|short| format!("-{short}")))
                .chain(
                    arg.get_long_and_visible_aliases()
                        .iter()
                        .flat_map(|longs| longs.iter().map(|short| format!("--{short}"))),
                )
                .map(|arg_str| (arg_str, arg.get_action().takes_values()))
                .collect::<Vec<_>>()
        })
        .collect::<HashMap<_, _>>();

    #[derive(Debug, Default)]
    struct SplitArgs {
        apk_args: Vec<String>,
        cargo_args: Vec<String>,
        next_takes_value: bool,
    }

    let split_args = input
        .into_iter()
        .fold(SplitArgs::default(), |mut split_args, elem| {
            let known_arg = known_args_taking_value.get(&elem);
            if known_arg.is_some() || split_args.next_takes_value {
                split_args.apk_args.push(elem)
            } else {
                split_args.cargo_args.push(elem)
            }
            split_args.next_takes_value = known_arg.copied().unwrap_or(false);
            split_args
        });

    let m = Args::command()
        .no_binary_name(true)
        .get_matches_from(&split_args.apk_args);
    let args =
        Args::from_arg_matches(&m).map_err(|e| anyhow::anyhow!("Failed to parse args: {}", e))?;
    Ok((args, split_args.cargo_args))
}

fn iterator_single_item<T>(mut iter: impl Iterator<Item = T>) -> Option<T> {
    let first_item = iter.next()?;
    if iter.next().is_some() {
        None
    } else {
        Some(first_item)
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let Cmd {
        apk: RapkCmd::Rapk { cmd },
    } = Cmd::parse();

    macro_rules! prepare {
        ($args:expr, $cmd:ident, $builder:ident) => {
            let $cmd = Subcommand::new($args.subcommand_args)?;
            let mut $builder = ApkBuilder::from_subcommand(&$cmd, $args.device)?;
            $builder.set_repro_flags(
                $args.deterministic,
                $args.unsigned,
                $args.align,
                $args.timestamp,
                $args.no_normalize_zip,
            );
        };
    }

    match cmd {
        RapkSubCmd::Check { args } => {
            prepare!(args, _cmd, builder);
            builder.check()?;
        }
        RapkSubCmd::Build { args } => {
            prepare!(args, cmd, builder);
            for artifact in cmd.artifacts() {
                builder.build(artifact)?;
            }
        }
        RapkSubCmd::Ndk {
            cargo_cmd,
            cargo_args,
        } => {
            let (args, cargo_args) = split_apk_and_cargo_args(cargo_args)?;
            prepare!(args, _cmd, builder);
            builder.default(&cargo_cmd, &cargo_args)?;
        }
        RapkSubCmd::Run { args, no_logcat } => {
            prepare!(args, cmd, builder);
            let artifact = iterator_single_item(cmd.artifacts()).ok_or(Error::invalid_args())?;
            builder.run(artifact, no_logcat)?;
        }
        RapkSubCmd::Gdb { args } => {
            prepare!(args, cmd, builder);
            let artifact = iterator_single_item(cmd.artifacts()).ok_or(Error::invalid_args())?;
            builder.gdb(artifact)?;
        }
        RapkSubCmd::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}

#[test]
fn test_split_apk_and_cargo_args() {
    // Set up a default because cargo-subcommand doesn't derive a default
    let args_default = Args::parse_from(std::iter::empty::<&str>());
    let check = |input: Vec<String>| split_apk_and_cargo_args(input).unwrap();

    assert_eq!(
        check(vec!["--quiet".into()]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec![]
        )
    );

    assert_eq!(
        check(vec!["unrecognized".to_string(), "--quiet".to_string()]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec!["unrecognized".to_string()]
        )
    );

    assert_eq!(
        check(vec!["--unrecognized".to_string(), "--quiet".to_string()]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec!["--unrecognized".to_string()]
        )
    );

    assert_eq!(
        check(vec!["-p".to_string(), "foo".to_string()]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    package: vec!["foo".to_string()],
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec![]
        )
    );

    assert_eq!(
        check(vec![
            "-p".to_string(),
            "foo".to_string(),
            "--unrecognized".to_string(),
            "--quiet".to_string()
        ]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    package: vec!["foo".to_string()],
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec!["--unrecognized".to_string()]
        )
    );

    assert_eq!(
        check(vec![
            "--no-deps".to_string(),
            "-p".to_string(),
            "foo".to_string(),
            "--unrecognized".to_string(),
            "--quiet".to_string()
        ]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    package: vec!["foo".to_string()],
                    ..args_default.subcommand_args.clone()
                },
                ..args_default.clone()
            },
            vec!["--no-deps".to_string(), "--unrecognized".to_string()]
        )
    );

    assert_eq!(
        check(vec![
            "--no-deps".to_string(),
            "--device".to_string(),
            "adb:test".to_string(),
            "--unrecognized".to_string(),
            "--quiet".to_string()
        ]),
        (
            Args {
                subcommand_args: cargo_subcommand::Args {
                    quiet: true,
                    ..args_default.subcommand_args.clone()
                },
                device: Some("adb:test".to_string()),
                ..args_default.clone()
            },
            vec!["--no-deps".to_string(), "--unrecognized".to_string()]
        )
    );
}
