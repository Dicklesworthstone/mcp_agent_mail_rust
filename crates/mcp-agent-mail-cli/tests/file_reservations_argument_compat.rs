//! Argument-compatibility regressions for the PR #331 review.
//!
//! IDs are integer lists; commas inside paths are filename/glob data. These
//! tests deliberately exercise the public parser without mutating a mailbox.

use clap::Parser;
use mcp_agent_mail_cli::{Cli, Commands, FileReservationsCommand};

fn parse_restrictions(
    verb: &str,
    filters: &[&str],
) -> Result<(Vec<String>, Vec<i64>), clap::Error> {
    let mut args = vec!["am", "file_reservations", verb, "test-project", "RedFox"];
    args.extend_from_slice(filters);
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        Some(Commands::FileReservations {
            action: FileReservationsCommand::Release { paths, ids, .. },
        })
        | Some(Commands::FileReservations {
            action: FileReservationsCommand::Renew { paths, ids, .. },
        }) => Ok((paths, ids)),
        other => panic!("expected release or renew, got {other:?}"),
    }
}

#[test]
fn file_reservations_ids_accept_csv_space_repetition_and_mixed_forms() {
    for verb in ["release", "renew"] {
        for filters in [
            vec!["--ids", "11,12,13"],
            vec!["--ids", "11", "12", "13"],
            vec!["--ids", "11", "--ids", "12", "--ids", "13"],
            vec!["--ids", "11,12", "--ids", "13"],
        ] {
            let (paths, ids) = parse_restrictions(verb, &filters).unwrap();
            assert!(paths.is_empty());
            assert_eq!(ids, [11, 12, 13], "{verb} {filters:?}");
        }
    }
}

#[test]
fn file_reservations_paths_accept_space_repetition_and_mixed_forms() {
    for verb in ["release", "renew"] {
        for filters in [
            vec!["--paths", "src/a.rs", "src/b.rs", "src/c.rs"],
            vec![
                "--paths", "src/a.rs", "--paths", "src/b.rs", "--paths", "src/c.rs",
            ],
            vec!["--paths", "src/a.rs", "src/b.rs", "--paths", "src/c.rs"],
        ] {
            let (paths, ids) = parse_restrictions(verb, &filters).unwrap();
            assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/c.rs"]);
            assert!(ids.is_empty());
        }
    }
}

#[test]
fn file_reservations_path_commas_and_brace_globs_remain_literal() {
    for verb in ["release", "renew"] {
        let expected = [
            "literal,comma.rs",
            "src/{one,two}.rs",
            "directory with spaces/file.rs",
            " leading-and-trailing-space.rs ",
        ];
        let mut filters = vec!["--paths"];
        filters.extend_from_slice(&expected);
        let (paths, ids) = parse_restrictions(verb, &filters).unwrap();
        assert_eq!(paths, expected);
        assert!(ids.is_empty());
    }
}

#[test]
fn file_reservations_unrestricted_arguments_remain_optional() {
    for verb in ["release", "renew"] {
        let (paths, ids) = parse_restrictions(verb, &[]).unwrap();
        assert!(paths.is_empty());
        assert!(ids.is_empty());
    }
}

#[test]
fn file_reservations_paths_and_ids_can_be_combined_in_either_order() {
    for verb in ["release", "renew"] {
        for filters in [
            vec!["--paths", "src/a.rs", "src/b.rs", "--ids", "11,12"],
            vec!["--ids", "11,12", "--paths", "src/a.rs", "src/b.rs"],
        ] {
            let (paths, ids) = parse_restrictions(verb, &filters).unwrap();
            assert_eq!(paths, ["src/a.rs", "src/b.rs"]);
            assert_eq!(ids, [11, 12]);
        }
    }
}

#[test]
fn file_reservations_invalid_integer_lists_are_rejected() {
    for verb in ["release", "renew"] {
        for value in ["", "11,x", "11,,12", "11,", "9223372036854775808"] {
            assert!(
                parse_restrictions(verb, &["--ids", value]).is_err(),
                "{verb} --ids {value:?} must not be accepted"
            );
        }
    }
}

#[test]
fn file_reservations_restriction_flags_require_a_value() {
    for verb in ["release", "renew"] {
        for flag in ["--paths", "--ids"] {
            assert!(
                parse_restrictions(verb, &[flag]).is_err(),
                "{verb} {flag} must not become an unrestricted command"
            );
        }
    }
}
