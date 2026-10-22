// SPDX-License-Identifier: GPL-3.0-or-later

//! `keep_gcodes`: keeps the print files at the top of a gcodes directory through a firmware
//! install that deletes them. Before the install it hard-links each top-level `.gcode` into a
//! hidden folder beside them, which adds no data and changes no file the printer has; after it,
//! the restore moves them into a visible folder and checks the count, and the finish screen
//! says what was kept.

use super::{StepCtx, StepOutcome};
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

const KEEP_TIMEOUT: Duration = Duration::from_secs(120);

/// Links every top-level `*.gcode` in `dir` into `dir/.helix-kept` and prints how many the
/// folder holds. A file already set aside under the same name is replaced by the current one.
/// Nothing to keep leaves no folder behind.
pub fn set_aside_script(dir: &str) -> String {
    format!(
        "cd {d} 2>/dev/null || {{ echo 0; exit 0; }}; \
         mkdir -p {k} || exit 1; \
         for f in *.gcode; do [ -f \"$f\" ] || continue; ln -f \"$f\" {k}/\"$f\" || exit 1; done; \
         n=0; for f in {k}/*.gcode; do [ -f \"$f\" ] && n=$((n+1)); done; \
         [ \"$n\" -gt 0 ] || rmdir {k} 2>/dev/null; \
         echo \"$n\"",
        d = sh_quote(dir),
        k = SET_ASIDE_DIR,
    )
}

/// Moves what the set-aside holds into `dir/into`, never over a file already there, and
/// prints how many `.gcode` files `into` holds and how many are still set aside.
pub fn restore_script(dir: &str, into: &str) -> String {
    format!(
        "cd {d} || exit 1; I={i}; \
         if [ -d {k} ]; then mkdir -p \"$I\" || exit 1; \
         for f in {k}/*.gcode; do [ -f \"$f\" ] || continue; b=${{f#{k}/}}; \
         [ -e \"$I/$b\" ] || mv \"$f\" \"$I/$b\"; done; \
         rmdir {k} 2>/dev/null; fi; \
         kept=0; for f in \"$I\"/*.gcode; do [ -f \"$f\" ] && kept=$((kept+1)); done; \
         left=0; for f in {k}/*.gcode; do [ -f \"$f\" ] && left=$((left+1)); done; \
         echo \"$kept $left\"",
        d = sh_quote(dir),
        i = sh_quote(into),
        k = SET_ASIDE_DIR,
    )
}

/// What the restore found, as its script prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// `.gcode` files in the visible folder.
    pub kept: u64,
    /// Files still in the hidden folder.
    pub left: u64,
}

pub fn parse_restored(out: &str) -> Option<Restored> {
    let mut it = out.split_whitespace().map(str::parse::<u64>);
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(kept)), Some(Ok(left)), None) => Some(Restored { kept, left }),
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

/// The finish screen's line for a restore that expected `expected` files (`None` when the
/// set-aside recorded nothing) and found `found` (`None` when it could not tell). No line when
/// there was nothing to keep.
pub fn restore_note(
    expected: Option<u64>,
    found: Option<Restored>,
    dir: &str,
    into: &str,
) -> Option<(String, bool)> {
    let hidden = format!("{}/{SET_ASIDE_DIR}", dir.trim_end_matches('/'));
    match (expected, found) {
        (None | Some(0), Some(Restored { kept: 0, left: 0 })) => None,
        (e, Some(r)) if r.left == 0 && r.kept >= e.unwrap_or(0) => Some((
            format!(
                "your {} from before the install were kept: they are in the folder \"{into}\"",
                files(r.kept)
            ),
            false,
        )),
        (e, Some(r)) => Some((
            format!(
                "the installer set aside {} before the install, and only {} reached the folder \
                 \"{into}\". The rest are still on the printer in {hidden}",
                files(e.unwrap_or(r.kept + r.left)),
                r.kept
            ),
            true,
        )),
        (e, None) => Some((
            format!(
                "the installer set aside {} before the install and could not move them back. \
                 They are still on the printer in {hidden}",
                e.map_or_else(|| "your print files".to_string(), files)
            ),
            true,
        )),
    }
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
        set_aside(ctx, dir).await
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
async fn set_aside(ctx: &mut StepCtx<'_>, dir: &str) -> StepOutcome {
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
    if n > 0 {
        ctx.log(format!(
            "set aside {} in {}/{SET_ASIDE_DIR}",
            files(n),
            dir.trim_end_matches('/')
        ));
    }
    StepOutcome::Done
}

/// Runs after the install. It never fails the run: what it could not do is a warning on the
/// finish screen, since the files stay safe where they are.
async fn put_back(ctx: &mut StepCtx<'_>, dir: &str, into: &str) -> StepOutcome {
    let expected = ctx.facts.get(KEPT_FACT).and_then(Value::as_u64);
    let found = match exec(ctx, &restore_script(dir, into)).await {
        Ok(out) => parse_restored(&out),
        Err(why) => {
            ctx.log(format!("moving the print files back failed: {why}"));
            None
        }
    };
    if let Some(r) = found {
        ctx.log(format!(
            "{} in {dir}/{into}, {} still set aside",
            files(r.kept),
            r.left
        ));
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

    const INTO: &str = "From stock firmware";

    #[test]
    fn files_set_aside_survive_the_top_level_delete_and_come_back_in_the_folder() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path().join("gcodes");
        fs::create_dir_all(g.join("sub")).unwrap();
        fs::write(g.join("a.gcode"), "A").unwrap();
        fs::write(g.join("b c.gcode"), "B").unwrap();
        fs::write(g.join("notes.txt"), "N").unwrap();
        fs::write(g.join("sub/d.gcode"), "D").unwrap();
        let dir = g.to_str().unwrap();

        assert_eq!(sh(&set_aside_script(dir)).trim(), "2");
        assert_eq!(fs::read_to_string(g.join("a.gcode")).unwrap(), "A");
        delete_top_level(&g);
        let r = parse_restored(&sh(&restore_script(dir, INTO))).unwrap();
        assert_eq!(r, Restored { kept: 2, left: 0 });
        assert_eq!(
            fs::read_to_string(g.join(INTO).join("a.gcode")).unwrap(),
            "A"
        );
        assert_eq!(
            fs::read_to_string(g.join(INTO).join("b c.gcode")).unwrap(),
            "B"
        );
        assert!(!g.join(SET_ASIDE_DIR).exists());
        assert!(g.join("sub/d.gcode").exists());
    }

    #[test]
    fn a_restore_run_again_finds_the_same_files_and_overwrites_nothing() {
        let t = tempfile::tempdir().unwrap();
        let g = t.path();
        fs::write(g.join("a.gcode"), "old").unwrap();
        let dir = g.to_str().unwrap();
        sh(&set_aside_script(dir));
        delete_top_level(g);
        sh(&restore_script(dir, INTO));
        fs::write(g.join(INTO).join("a.gcode"), "edited since").unwrap();
        // A stale set-aside of the same name must not replace the newer file.
        fs::create_dir(g.join(SET_ASIDE_DIR)).unwrap();
        fs::write(g.join(SET_ASIDE_DIR).join("a.gcode"), "stale").unwrap();

        let r = parse_restored(&sh(&restore_script(dir, INTO))).unwrap();
        assert_eq!(r, Restored { kept: 1, left: 1 });
        assert_eq!(
            fs::read_to_string(g.join(INTO).join("a.gcode")).unwrap(),
            "edited since"
        );
    }

    #[test]
    fn nothing_to_keep_leaves_no_folder_and_a_missing_dir_keeps_nothing() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().to_str().unwrap();
        assert_eq!(sh(&set_aside_script(dir)).trim(), "0");
        assert!(!t.path().join(SET_ASIDE_DIR).exists());
        let missing = t.path().join("none");
        assert_eq!(sh(&set_aside_script(missing.to_str().unwrap())).trim(), "0");
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
    fn the_finish_line_says_what_was_kept_or_warns_where_the_rest_are() {
        let ok = |k, l| Some(Restored { kept: k, left: l });
        assert_eq!(restore_note(None, ok(0, 0), "/g", INTO), None);
        assert_eq!(restore_note(Some(0), ok(0, 0), "/g", INTO), None);
        let (text, warn) = restore_note(Some(1), ok(1, 0), "/g", INTO).unwrap();
        assert!(!warn);
        assert!(text.contains("1 print file from"), "{text}");
        let (text, warn) = restore_note(Some(3), ok(2, 0), "/g/", INTO).unwrap();
        assert!(warn);
        assert!(
            text.contains("3 print files") && text.contains("/g/.helix-kept"),
            "{text}"
        );
        let (_, warn) = restore_note(Some(3), ok(3, 1), "/g", INTO).unwrap();
        assert!(warn);
        let (text, warn) = restore_note(Some(3), None, "/g", INTO).unwrap();
        assert!(warn && text.contains("3 print files"), "{text}");
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn restore_output_parses_only_two_counts() {
        assert_eq!(parse_restored("4 0\n"), Some(Restored { kept: 4, left: 0 }));
        assert_eq!(parse_restored("4"), None);
        assert_eq!(parse_restored("4 0 1"), None);
        assert_eq!(parse_restored("x 0"), None);
    }
}
