// SPDX-License-Identifier: GPL-3.0-or-later

//! `keep_gcodes`: keeps the print files at the top of a gcodes directory through a firmware
//! install that deletes them. Before the install it hard-links each top-level `.gcode` into a
//! hidden folder beside them, which adds no data and changes no file the printer has. The
//! restore moves whatever that folder holds into a visible folder and checks the count; any
//! recipe for the printer may run it, so a run that stopped part-way is put right by the next.

use super::{StepCtx, StepOutcome};
use crate::facts::Facts;
use crate::run_state::FinishNote;
use crate::runner::Input;
use crate::transport::ssh_russh::sh_quote;
use serde_json::{json, Value};
use std::time::Duration;

/// The hidden folder inside the gcodes directory. Moonraker lists no dot-folder, and a link in
/// the same directory tree is on the same filesystem, so `ln` works without copying.
pub const SET_ASIDE_DIR: &str = ".helix-kept";

/// How many files the set-aside holds, for the restore to check against.
pub const KEPT_FACT: &str = "kept_gcodes";
/// The gcodes directory and the folder the files go back into, for the recovery screen.
pub const KEPT_DIR_FACT: &str = "kept_gcodes_dir";
pub const KEPT_INTO_FACT: &str = "kept_gcodes_into";
/// Set once a restore found nothing left set aside.
pub const BACK_FACT: &str = "kept_gcodes_back";

const KEEP_TIMEOUT: Duration = Duration::from_secs(120);

const RUN_AGAIN: &str = "Run the installer on this printer again and it puts them back";

/// Links every top-level `*.gcode` in `dir` into `dir/.helix-kept` and prints how many the
/// folder holds. A file already set aside is never replaced: the same file is skipped and a
/// different one of the same name is linked as `name (2).gcode`. Nothing to keep leaves no
/// folder behind.
pub fn set_aside_script(dir: &str) -> String {
    format!(
        "cd {d} 2>/dev/null || {{ echo 0; exit 0; }}; \
         mkdir -p {k} || exit 1; \
         for f in ./*.gcode; do [ -f \"$f\" ] || continue; b=${{f#./}}; t={k}/\"$b\"; i=2; \
         while [ -e \"$t\" ] && ! [ \"$f\" -ef \"$t\" ]; do t={k}/\"${{b%.gcode}} ($i).gcode\"; i=$((i+1)); done; \
         [ -e \"$t\" ] || ln \"$f\" \"$t\" || exit 1; done; \
         n=0; for f in {k}/*.gcode; do [ -f \"$f\" ] && n=$((n+1)); done; \
         [ \"$n\" -gt 0 ] || rmdir {k} 2>/dev/null; \
         echo \"$n\"",
        d = sh_quote(dir),
        k = SET_ASIDE_DIR,
    )
}

/// Moves what the set-aside holds into `dir/into`, never over a file already there (a name
/// taken gets ` (2)`), and prints whether a set-aside was there, how many `.gcode` files `into`
/// holds and how many are still set aside; `gone` when `dir` itself is missing.
pub fn restore_script(dir: &str, into: &str) -> String {
    format!(
        "cd {d} 2>/dev/null || {{ echo gone; exit 0; }}; I={i}; had=0; \
         if [ -d {k} ]; then had=1; mkdir -p \"$I\" || exit 1; \
         for f in {k}/*.gcode; do [ -f \"$f\" ] || continue; b=${{f#{k}/}}; t=\"$I/$b\"; i=2; \
         while [ -e \"$t\" ]; do t=\"$I/${{b%.gcode}} ($i).gcode\"; i=$((i+1)); done; \
         mv \"$f\" \"$t\"; done; \
         rmdir {k} 2>/dev/null; fi; \
         kept=0; for f in \"$I\"/*.gcode; do [ -f \"$f\" ] && kept=$((kept+1)); done; \
         left=0; for f in {k}/*.gcode; do [ -f \"$f\" ] && left=$((left+1)); done; \
         echo \"$had $kept $left\"",
        d = sh_quote(dir),
        i = sh_quote(into),
        k = SET_ASIDE_DIR,
    )
}

/// What the restore found, as its script prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// The gcodes directory is missing.
    Gone,
    Found {
        /// A set-aside folder was there when the restore started.
        had: bool,
        /// `.gcode` files in the visible folder.
        kept: u64,
        /// Files still in the hidden folder.
        left: u64,
    },
}

pub fn parse_restored(out: &str) -> Option<Restored> {
    let words: Vec<&str> = out.split_whitespace().collect();
    match words.as_slice() {
        ["gone"] => Some(Restored::Gone),
        [had @ ("0" | "1"), kept, left] => Some(Restored::Found {
            had: *had == "1",
            kept: kept.parse().ok()?,
            left: left.parse().ok()?,
        }),
        _ => None,
    }
}

fn files(n: u64) -> String {
    if n == 1 {
        "1 print file".into()
    } else {
        format!("{n} print files")
    }
}

fn hidden(dir: &str) -> String {
    format!("{}/{SET_ASIDE_DIR}", dir.trim_end_matches('/'))
}

/// The finish screen's line for a restore. `expected` is what this run set aside (`None` when it
/// set nothing aside: a run after one that stopped part-way), `found` what the restore saw
/// (`None` when it could not run). No line when nothing was set aside and none was found.
pub fn restore_note(
    expected: Option<u64>,
    found: Option<Restored>,
    dir: &str,
    into: &str,
) -> Option<(String, bool)> {
    let expected = expected.filter(|&n| n > 0);
    let kept_line = |kept| {
        (
            format!(
                "your {} from before the install were kept: they are in the folder \"{into}\"",
                files(kept)
            ),
            false,
        )
    };
    let missing = |e: u64, kept| {
        (
            format!(
                "the installer set aside {} before the install, and only {kept} are in the folder \
                 \"{into}\". The others could not be found on the printer",
                files(e)
            ),
            true,
        )
    };
    match found {
        None => expected.map(|e| {
            (
                format!(
                    "the installer could not move back the {} it set aside. They should still be \
                     on the printer in {}. {RUN_AGAIN}",
                    files(e),
                    hidden(dir)
                ),
                true,
            )
        }),
        Some(Restored::Gone) => expected.map(|e| {
            (
                format!(
                    "the {} the installer set aside could not be found: {dir} is no longer on \
                     the printer",
                    files(e)
                ),
                true,
            )
        }),
        Some(Restored::Found { left, .. }) if left > 0 => Some((
            format!(
                "{} the installer set aside could not be moved and are still on the printer in \
                 {}. {RUN_AGAIN}",
                files(left),
                hidden(dir)
            ),
            true,
        )),
        Some(Restored::Found { had, kept, .. }) => match expected {
            Some(e) if kept < e => Some(missing(e, kept)),
            Some(_) => Some(kept_line(kept)),
            None if had => Some(kept_line(kept)),
            None => None,
        },
    }
}

/// Files this run set aside that no restore has put back yet: their count and where they are.
pub fn set_aside_pending(facts: &Facts) -> Option<(u64, String)> {
    let n = facts
        .get(KEPT_FACT)
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)?;
    if facts.get(BACK_FACT) == Some(&Value::Bool(true)) {
        return None;
    }
    let dir = facts.get(KEPT_DIR_FACT).and_then(Value::as_str)?;
    Some((n, hidden(dir)))
}

pub async fn run(
    ctx: &mut StepCtx<'_>,
    dir: &str,
    into: &str,
    restore: bool,
    input: Option<Input>,
) -> StepOutcome {
    if let Some(ask) = ctx.need_login(input).await {
        return ask;
    }
    if restore {
        put_back(ctx, dir, into).await
    } else {
        set_aside(ctx, dir, into).await
    }
}

async fn exec(ctx: &mut StepCtx<'_>, cmd: &str) -> Result<String, String> {
    let ran = match ctx.ssh().await {
        Ok(s) => s.exec(cmd, None, KEEP_TIMEOUT).await,
        Err(e) => Err(e),
    };
    match ran {
        Ok(out) if out.exit == 0 => Ok(out.stdout),
        Ok(out) => Err(format!("exit {}: {}", out.exit, out.stderr.trim())),
        Err(e) => {
            ctx.drop_ssh();
            Err(e.to_string())
        }
    }
}

/// Runs before the point of no return: a failure stops the run with every file where it was.
async fn set_aside(ctx: &mut StepCtx<'_>, dir: &str, into: &str) -> StepOutcome {
    let n = match exec(ctx, &set_aside_script(dir)).await {
        Ok(out) => out.trim().parse::<u64>().ok(),
        Err(why) => {
            ctx.log(format!("setting the print files aside failed: {why}"));
            None
        }
    };
    let Some(n) = n else {
        return StepOutcome::Failed {
            reason: format!(
                "the installer could not set aside the print files in {dir}, so it stopped \
                 before the install that would delete them"
            ),
            help: Some(format!(
                "Copy the .gcode files in {dir} to your computer, then try this step again."
            )),
        };
    };
    ctx.facts.insert(KEPT_FACT.into(), json!(n));
    ctx.facts.insert(KEPT_DIR_FACT.into(), json!(dir));
    ctx.facts.insert(KEPT_INTO_FACT.into(), json!(into));
    ctx.facts.remove(BACK_FACT);
    if n > 0 {
        ctx.log(format!("set aside {} in {}", files(n), hidden(dir)));
    }
    StepOutcome::Done
}

/// Never fails the run: what it could not do is a warning on the finish screen, and the files
/// stay where the next run's restore finds them.
async fn put_back(ctx: &mut StepCtx<'_>, dir: &str, into: &str) -> StepOutcome {
    let expected = ctx.facts.get(KEPT_FACT).and_then(Value::as_u64);
    let found = match exec(ctx, &restore_script(dir, into)).await {
        Ok(out) => parse_restored(&out),
        Err(why) => {
            ctx.log(format!("moving the print files back failed: {why}"));
            None
        }
    };
    match found {
        Some(Restored::Found { had, kept, left }) => {
            if had || expected.is_some_and(|n| n > 0) {
                ctx.log(format!(
                    "{} in {dir}/{into}, {left} still set aside",
                    files(kept)
                ));
            }
            if left == 0 {
                ctx.facts.insert(BACK_FACT.into(), json!(true));
            }
        }
        Some(Restored::Gone) => {
            ctx.log(format!("{dir} is missing"));
            ctx.facts.insert(BACK_FACT.into(), json!(true));
        }
        None => {}
    }
    let step = ctx.step;
    ctx.finish_notes.retain(|n| n.step != step);
    if let Some((text, warn)) = restore_note(expected, found, dir, into) {
        ctx.finish_notes.push(FinishNote { step, text, warn });
    }
    StepOutcome::Done
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn sh(script: &str) -> String {
        let out = Command::new("sh").arg("-c").arg(script).output().unwrap();
        assert!(out.status.success(), "{script}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// What the firmware install does to the gcodes directory: every top-level `.gcode` goes.
    fn delete_top_level(dir: &Path) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() && p.extension().is_some_and(|x| x == "gcode") {
                fs::remove_file(p).unwrap();
            }
        }
    }

    fn read(p: impl AsRef<Path>) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn found(had: bool, kept: u64, left: u64) -> Restored {
        Restored::Found { had, kept, left }
    }

    const INTO: &str = "From stock firmware";

    #[test]
    fn files_set_aside_survive_the_top_level_delete_and_come_back_in_the_folder() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path().join("gcodes");
        fs::create_dir_all(g.join("sub")).unwrap();
        fs::write(g.join("a.gcode"), "A").unwrap();
        fs::write(g.join("b c.gcode"), "B").unwrap();
        fs::write(g.join("-dash.gcode"), "D").unwrap();
        fs::write(g.join("notes.txt"), "N").unwrap();
        fs::write(g.join("sub/d.gcode"), "S").unwrap();
        let dir = g.to_str().unwrap();

        assert_eq!(sh(&set_aside_script(dir)).trim(), "3");
        assert_eq!(read(g.join("a.gcode")), "A");
        delete_top_level(&g);
        let r = parse_restored(&sh(&restore_script(dir, INTO))).unwrap();
        assert_eq!(r, found(true, 3, 0));
        assert_eq!(read(g.join(INTO).join("a.gcode")), "A");
        assert_eq!(read(g.join(INTO).join("b c.gcode")), "B");
        assert_eq!(read(g.join(INTO).join("-dash.gcode")), "D");
        assert!(!g.join(SET_ASIDE_DIR).exists());
        assert!(g.join("sub/d.gcode").exists());
    }

    #[test]
    fn a_set_aside_never_replaces_a_file_already_kept() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path();
        let dir = g.to_str().unwrap();
        fs::write(g.join("benchy.gcode"), "original").unwrap();
        sh(&set_aside_script(dir));
        // The top level is deleted, then a different file of the same name is uploaded.
        delete_top_level(g);
        fs::write(g.join("benchy.gcode"), "new upload").unwrap();

        assert_eq!(sh(&set_aside_script(dir)).trim(), "2");
        assert_eq!(read(g.join(SET_ASIDE_DIR).join("benchy.gcode")), "original");
        assert_eq!(
            read(g.join(SET_ASIDE_DIR).join("benchy (2).gcode")),
            "new upload"
        );
    }

    #[test]
    fn setting_aside_twice_counts_each_file_once() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a.gcode"), "A").unwrap();
        let dir = t.path().to_str().unwrap();
        sh(&set_aside_script(dir));
        assert_eq!(sh(&set_aside_script(dir)).trim(), "1");
    }

    #[test]
    fn a_restore_never_overwrites_and_a_second_restore_finds_nothing_to_move() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path();
        fs::write(g.join("a.gcode"), "old").unwrap();
        let dir = g.to_str().unwrap();
        sh(&set_aside_script(dir));
        delete_top_level(g);
        sh(&restore_script(dir, INTO));
        fs::write(g.join(INTO).join("a.gcode"), "edited since").unwrap();
        fs::create_dir(g.join(SET_ASIDE_DIR)).unwrap();
        fs::write(g.join(SET_ASIDE_DIR).join("a.gcode"), "stale").unwrap();

        let r = parse_restored(&sh(&restore_script(dir, INTO))).unwrap();
        assert_eq!(r, found(true, 2, 0));
        assert_eq!(read(g.join(INTO).join("a.gcode")), "edited since");
        assert_eq!(read(g.join(INTO).join("a (2).gcode")), "stale");
        let r = parse_restored(&sh(&restore_script(dir, INTO))).unwrap();
        assert_eq!(r, found(false, 2, 0));
    }

    #[test]
    fn nothing_to_keep_leaves_no_folder_and_a_missing_dir_keeps_nothing() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().to_str().unwrap();
        assert_eq!(sh(&set_aside_script(dir)).trim(), "0");
        assert!(!t.path().join(SET_ASIDE_DIR).exists());
        let missing = t.path().join("none");
        let missing = missing.to_str().unwrap();
        assert_eq!(sh(&set_aside_script(missing)).trim(), "0");
        assert_eq!(
            parse_restored(&sh(&restore_script(missing, INTO))),
            Some(Restored::Gone)
        );
        assert_eq!(
            parse_restored(&sh(&restore_script(dir, INTO))),
            Some(found(false, 0, 0))
        );
    }

    #[test]
    fn the_finish_line_says_what_was_kept() {
        let (text, warn) = restore_note(Some(1), Some(found(true, 1, 0)), "/g", INTO).unwrap();
        assert!(!warn);
        assert!(text.contains("1 print file from"), "{text}");
        // A later run that finds an earlier run's set-aside says what it put back.
        let (text, warn) = restore_note(None, Some(found(true, 4, 0)), "/g", INTO).unwrap();
        assert!(!warn && text.contains("4 print files"), "{text}");
        // A resumed run whose restore already ran still reports it.
        let (_, warn) = restore_note(Some(2), Some(found(false, 2, 0)), "/g", INTO).unwrap();
        assert!(!warn);
    }

    #[test]
    fn nothing_set_aside_and_nothing_found_is_no_line() {
        assert_eq!(
            restore_note(None, Some(found(false, 0, 0)), "/g", INTO),
            None
        );
        assert_eq!(
            restore_note(Some(0), Some(found(false, 3, 0)), "/g", INTO),
            None
        );
        assert_eq!(
            restore_note(None, Some(found(false, 3, 0)), "/g", INTO),
            None
        );
        assert_eq!(restore_note(None, None, "/g", INTO), None);
        assert_eq!(restore_note(None, Some(Restored::Gone), "/g", INTO), None);
    }

    #[test]
    fn files_that_are_gone_are_never_said_to_be_safe() {
        for f in [Some(found(true, 2, 0)), Some(found(false, 2, 0))] {
            let (text, warn) = restore_note(Some(3), f, "/g", INTO).unwrap();
            assert!(warn);
            assert!(text.contains("could not be found"), "{text}");
            assert!(!text.contains(".helix-kept"), "{text}");
        }
        let (text, warn) = restore_note(Some(3), Some(Restored::Gone), "/g", INTO).unwrap();
        assert!(warn && text.contains("could not be found"), "{text}");
    }

    #[test]
    fn files_still_set_aside_are_named_with_how_to_get_them_back() {
        let (text, warn) = restore_note(Some(3), Some(found(true, 2, 1)), "/g/", INTO).unwrap();
        assert!(warn);
        assert!(
            text.contains("/g/.helix-kept") && text.contains(RUN_AGAIN),
            "{text}"
        );
        let (text, warn) = restore_note(Some(3), None, "/g", INTO).unwrap();
        assert!(
            warn && text.contains("3 print files") && text.contains(RUN_AGAIN),
            "{text}"
        );
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn pending_until_a_restore_finds_nothing_left() {
        let mut f = Facts::new();
        assert_eq!(set_aside_pending(&f), None);
        f.insert(KEPT_FACT.into(), json!(3));
        f.insert(KEPT_DIR_FACT.into(), json!("/g"));
        assert_eq!(set_aside_pending(&f), Some((3, "/g/.helix-kept".into())));
        f.insert(BACK_FACT.into(), json!(true));
        assert_eq!(set_aside_pending(&f), None);
        f.insert(KEPT_FACT.into(), json!(0));
        f.remove(BACK_FACT);
        assert_eq!(set_aside_pending(&f), None);
    }

    #[test]
    fn restore_output_parses() {
        assert_eq!(parse_restored("1 4 0\n"), Some(found(true, 4, 0)));
        assert_eq!(parse_restored("gone\n"), Some(Restored::Gone));
        assert_eq!(parse_restored("4 0"), None);
        assert_eq!(parse_restored("2 4 0"), None);
        assert_eq!(parse_restored("1 x 0"), None);
    }
}
