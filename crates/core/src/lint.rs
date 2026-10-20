// SPDX-License-Identifier: GPL-3.0-or-later

//! Static checks on a manifest, run in CI before it is signed and published.

use crate::cache::sha256_hex;
use crate::manifest::{Cond, Manifest, Printer, RecipeChoice, Step, Verified, WaitTarget};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintError {
    /// The printer, recipe or artifact id the problem is in.
    pub subject: String,
    pub message: String,
}

impl fmt::Display for LintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.subject, self.message)
    }
}

/// How much hardware evidence a manifest must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Recipes not yet run on hardware.
    Staging,
    /// Every offered recipe has a verified run at its current digest, or awaits one.
    Published,
    /// As published, with nothing awaiting a run.
    Signing,
}

/// Strict lint, for the published list: every printer carries a `verified` record whose runs
/// cover each recipe it offers as that recipe now reads (or name it as awaiting a run), and no
/// image is an empty placeholder.
pub fn lint(m: &Manifest, asset_root: &Path) -> Vec<LintError> {
    lint_at(m, asset_root, Level::Published)
}

/// The published lint, with every recipe awaiting a hardware run an error: what is signed.
pub fn lint_for_signing(m: &Manifest, asset_root: &Path) -> Vec<LintError> {
    lint_at(m, asset_root, Level::Signing)
}

pub fn lint_with(m: &Manifest, asset_root: &Path, allow_unverified: bool) -> Vec<LintError> {
    let level = if allow_unverified {
        Level::Staging
    } else {
        Level::Published
    };
    lint_at(m, asset_root, level)
}

fn lint_at(m: &Manifest, asset_root: &Path, level: Level) -> Vec<LintError> {
    let allow_unverified = level == Level::Staging;
    let mut errs = Vec::new();
    let mut err = |subject: &str, message: String| {
        errs.push(LintError {
            subject: subject.to_string(),
            message,
        })
    };

    let restores: BTreeSet<&str> = m
        .printers
        .iter()
        .filter_map(|p| p.restore.as_ref())
        .flat_map(|r| r.recipes())
        .collect();

    for (id, a) in &m.artifacts {
        if a.sha256.len() != 64
            || !a
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            err(id, "sha256 must be 64 characters of lowercase hex".into());
        }
        if !a.url.starts_with("https://") {
            err(id, "url must be https".into());
        }
    }

    for p in &m.printers {
        lint_printer(m, p, asset_root, level, &mut err);
    }

    for (id, steps) in &m.recipes {
        lint_recipe(
            m,
            id,
            steps,
            restores.contains(id.as_str()),
            asset_root,
            !allow_unverified,
            &mut err,
        );
    }
    errs
}

fn lint_printer(
    m: &Manifest,
    p: &Printer,
    asset_root: &Path,
    level: Level,
    err: &mut impl FnMut(&str, String),
) {
    let allow_unverified = level == Level::Staging;
    if !p.fallback.refuse {
        err(&p.id, "fallback.refuse must be true".into());
    }
    if p.fallback.doc.trim().is_empty() {
        err(&p.id, "fallback.doc is empty".into());
    }
    if let Some(photo) = &p.photo {
        check_image(&p.id, photo, asset_root, !allow_unverified, err);
    }
    // A refused printer only turns printers away: nothing installs, so nothing to restore
    // or verify.
    if p.refused {
        if !p.recipes.is_empty() || p.restore.is_some() {
            err(
                &p.id,
                "a refused printer has no recipes and no restore".into(),
            );
        }
        return;
    }
    match &p.restore {
        None => err(&p.id, "printer has no restore recipe".into()),
        Some(r) => {
            for name in r.recipes() {
                if !m.recipes.contains_key(name) {
                    err(&p.id, format!("restore names unknown recipe {name:?}"));
                }
            }
        }
    }
    match &p.verified {
        None if !allow_unverified => err(&p.id, "printer has no verified record".into()),
        Some(v) => {
            for run in &v.runs {
                if !p.recipes.iter().any(|c| c.recipe == run.recipe) {
                    err(
                        &p.id,
                        format!(
                            "verified run on {:?} names {:?}, which no choice offers",
                            run.printer, run.recipe
                        ),
                    );
                }
            }
            if !allow_unverified {
                lint_runs_cover_recipes(m, p, v, level, err);
            }
        }
        None => {}
    }
    for c in &p.recipes {
        if c.when.is_empty() {
            err(
                &p.id,
                format!(
                    "choice for {:?} has an empty when and never matches",
                    c.recipe
                ),
            );
        }
        if !m.recipes.contains_key(&c.recipe) {
            err(&p.id, format!("choice names unknown recipe {:?}", c.recipe));
        }
    }
    for (i, a) in p.recipes.iter().enumerate() {
        for b in &p.recipes[i + 1..] {
            if choices_overlap(a, b) {
                err(
                    &p.id,
                    format!(
                        "recipe choices {:?} and {:?} can match the same facts",
                        a.recipe, b.recipe
                    ),
                );
            }
        }
    }
}

/// Step fields that only say something to the user: a hardware run cannot tell good words from
/// bad, so changing them does not need another run.
const PROSE: &[&str] = &[
    "text",
    "message",
    "help",
    "reason",
    "doc",
    "image",
    "password_hint",
    "question",
    "label",
];

/// The sha256 of what a recipe does: its steps with the user-facing words left out, and with
/// every field at its serde default (false, absent, empty) left out, so a field added to the
/// schema later does not change it. A verified run records it; any other change to the steps
/// makes the recipe unrun again.
pub fn behavior_sha256(steps: &[Step]) -> String {
    fn is_default(v: &Value) -> bool {
        match v {
            Value::Null | Value::Bool(false) => true,
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        }
    }
    fn strip(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.retain(|k, _| !PROSE.contains(&k.as_str()));
                map.values_mut().for_each(strip);
                map.retain(|_, v| !is_default(v));
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(steps).unwrap_or_default();
    strip(&mut v);
    sha256_hex(&serde_json::to_vec(&v).unwrap_or_default())
}

/// Each recipe the printer offers, as it does now, must be one a verified run ran: a recipe
/// whose behavior changed after its run counts as never run. One named in `awaiting_rerun`
/// passes until the list is signed.
fn lint_runs_cover_recipes(
    m: &Manifest,
    p: &Printer,
    v: &Verified,
    level: Level,
    err: &mut impl FnMut(&str, String),
) {
    let mut seen = BTreeSet::new();
    for id in p
        .recipes
        .iter()
        .map(|c| c.recipe.as_str())
        .filter(|id| seen.insert(*id))
    {
        let Some(steps) = m.recipes.get(id) else {
            continue;
        };
        let digest = behavior_sha256(steps);
        let ran = v
            .runs
            .iter()
            .any(|r| r.recipe == id && r.behavior_sha256 == digest);
        let awaiting = v.awaiting_rerun.iter().any(|a| a == id);
        match (ran, awaiting) {
            (true, true) => err(
                &p.id,
                format!("{id:?} is marked awaiting_rerun but a verified run covers it ({digest})"),
            ),
            (false, true) if level == Level::Signing => err(
                &p.id,
                format!(
                    "{id:?} is awaiting a hardware run at {digest}, so the list cannot be signed"
                ),
            ),
            (false, false) if v.runs.iter().any(|r| r.recipe == id) => err(
                &p.id,
                format!("{id:?} changed since its verified run: no run has its digest {digest}"),
            ),
            (false, false) => err(
                &p.id,
                format!("{id:?} has no verified run at its digest {digest}"),
            ),
            _ => {}
        }
    }
    for a in &v.awaiting_rerun {
        if !p.recipes.iter().any(|c| &c.recipe == a) {
            err(
                &p.id,
                format!("awaiting_rerun names {a:?}, which no choice offers"),
            );
        }
    }
}

/// Two choices overlap when they constrain the same keys and every key is satisfiable by one
/// fact at once. Choices with different key sets are never reported, even where one is
/// strictly narrower than the other, so this under-reports; the first matching choice wins
/// in `identify`, so an order-dependent pair is not ambiguous at runtime.
fn choices_overlap(a: &RecipeChoice, b: &RecipeChoice) -> bool {
    if a.when.is_empty() || a.when.keys().ne(b.when.keys()) {
        return false;
    }
    a.when
        .iter()
        .all(|(k, ca)| jointly_satisfiable(ca, &b.when[k]))
}

/// Pairs this cannot decide (a range against text conditions, say) count as satisfiable, so an
/// overlap is reported rather than missed.
fn jointly_satisfiable(a: &Cond, b: &Cond) -> bool {
    match (a, b) {
        (Cond::Contains { .. } | Cond::ContainsAll { .. }, _)
        | (_, Cond::Contains { .. } | Cond::ContainsAll { .. }) => true,
        (Cond::Exact(x), Cond::Exact(y)) => x == y,
        (Cond::Prefix { prefix }, Cond::Exact(x)) | (Cond::Exact(x), Cond::Prefix { prefix }) => {
            x.starts_with(prefix.as_str())
        }
        (Cond::Prefix { prefix: p }, Cond::Prefix { prefix: q }) => {
            p.starts_with(q.as_str()) || q.starts_with(p.as_str())
        }
        (Cond::Not { not }, Cond::Exact(x)) | (Cond::Exact(x), Cond::Not { not }) => x != not,
        (Cond::Not { .. }, _) | (_, Cond::Not { .. }) => true,
        (Cond::Prefix { .. }, Cond::Range(_)) | (Cond::Range(_), Cond::Prefix { .. }) => true,
        (Cond::Range(x), Cond::Range(y)) => x
            .sample_versions()
            .into_iter()
            .chain(y.sample_versions())
            .any(|v| x.matches(&v) && y.matches(&v)),
        _ => false,
    }
}

/// `published` also refuses an empty file: staging keeps 0-byte placeholders for pictures not
/// drawn yet, and a published list must not show one as a broken image.
fn check_image(
    subject: &str,
    rel: &str,
    root: &Path,
    published: bool,
    err: &mut impl FnMut(&str, String),
) {
    let path = Path::new(rel);
    let escapes = path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
    if escapes {
        err(
            subject,
            format!("image {rel:?} must be a relative path inside the asset root"),
        );
    } else if !root.join(path).is_file() {
        err(
            subject,
            format!("image {rel:?} not found under the asset root"),
        );
    } else if published && root.join(path).metadata().is_ok_and(|m| m.len() == 0) {
        err(subject, format!("image {rel:?} is an empty placeholder"));
    }
}

/// An install or update is done only once HelixScreen is seen running, so it ends waiting for
/// it. A restore may end with the step that removes HelixScreen or puts the stock screen back.
/// A panel flashed over USB is seen running by its `serial_flash` step, which reads the version
/// it boots; an info step after it may tell the user what to do on the panel next.
fn ends_done(steps: &[Step], is_restore: bool) -> bool {
    match steps.last() {
        Some(Step::Info { .. }) => steps
            .iter()
            .rev()
            .skip(1)
            .find(|s| !matches!(s, Step::Info { .. }))
            .is_some_and(|s| matches!(s, Step::SerialFlash { .. })),
        Some(last) => is_terminal(last, is_restore),
        None => false,
    }
}

fn is_terminal(s: &Step, is_restore: bool) -> bool {
    match s {
        Step::WaitFor { target, .. } => *target == WaitTarget::HelixService,
        Step::SerialFlash { .. } => true,
        Step::HelixInstall { .. }
        | Step::MoonrakerUpdate { .. }
        | Step::Require { .. }
        | Step::SshRun { .. }
        | Step::MoonrakerGcode { .. } => is_restore,
        _ => false,
    }
}

/// The first step that changes the printer, which the commit must come at or before so a
/// failure from there on is shown as a half-changed printer (Recovery), never "nothing was
/// changed". Commands, G-code, Moonraker updates and installs change it. Writing a stick does
/// not: it only touches the user's drive. The first user action after a stick is written is
/// where the user puts the stick in and the printer flashes itself, so it counts as a change.
fn first_change(steps: &[Step]) -> Option<usize> {
    let mut stick_written = false;
    steps.iter().position(|s| match s {
        Step::SshRun { .. }
        | Step::MoonrakerGcode { .. }
        | Step::MoonrakerUpdate { .. }
        | Step::HelixInstall {
            dry_run: false,
            check: false,
            ..
        }
        | Step::SerialFlash { .. } => true,
        Step::WriteStick { .. } => {
            stick_written = true;
            false
        }
        Step::UserAction { .. } => stick_written,
        _ => false,
    })
}

/// A `helix_install` resolves its release, and refuses one it cannot install, before it acts. One
/// after the point of no return would refuse a half-changed printer, so each must be covered by
/// a `helix_install` before the commit that resolves the same release: the same platform and
/// channel, as a check (a new install) or a dry run of the same mode.
fn release_checked_before(
    id: &str,
    steps: &[Step],
    commit: usize,
    err: &mut impl FnMut(&str, String),
) {
    for (i, s) in steps.iter().enumerate().skip(commit + 1) {
        let Some(install) = InstallRelease::of(s) else {
            continue;
        };
        let covered = steps[..commit]
            .iter()
            .filter_map(InstallRelease::of)
            .any(|before| before == install);
        if !covered {
            err(
                id,
                format!(
                    "step {i} installs HelixScreen after the point of no return (step {commit}), \
                     but no helix_install before the commit checks the same release (platform, \
                     channel and mode); add one with check: true"
                ),
            );
        }
    }
}

/// What decides the release a `helix_install` resolves: its platform, channel and mode. A
/// check resolves a new install.
#[derive(PartialEq)]
struct InstallRelease {
    platform: serde_json::Value,
    channel: Option<crate::manifest::Channel>,
    update: bool,
    uninstall: bool,
}

impl InstallRelease {
    fn of(s: &Step) -> Option<InstallRelease> {
        match s {
            Step::HelixInstall {
                platform,
                channel,
                update,
                uninstall,
                ..
            } => Some(InstallRelease {
                platform: serde_json::to_value(platform).unwrap_or_default(),
                channel: *channel,
                update: *update,
                uninstall: *uninstall,
            }),
            _ => None,
        }
    }
}

/// After the commit the runner only lets a step act on the printer once probes since the
/// commit or the last `wait_for` identify it again; a recipe without such a probe stops there
/// on every printer.
fn stale_after_commit(id: &str, steps: &[Step], commit: usize, err: &mut impl FnMut(&str, String)) {
    let mut probed = false;
    for (i, s) in steps.iter().enumerate().skip(commit + 1) {
        match s {
            Step::Probe { .. } => probed = true,
            Step::WaitFor { .. } => probed = false,
            _ if s.acts_on_printer() && !probed => err(
                id,
                format!(
                    "step {i} acts on the printer after the commit, but no probe since the \
                     commit or the last wait_for re-identifies it"
                ),
            ),
            _ => {}
        }
    }
}

fn lint_recipe(
    m: &Manifest,
    id: &str,
    steps: &[Step],
    is_restore: bool,
    asset_root: &Path,
    published: bool,
    err: &mut impl FnMut(&str, String),
) {
    match steps.last() {
        None => err(id, "recipe has no steps".into()),
        Some(_) if !ends_done(steps, is_restore) && is_restore => err(
            id,
            "last step does not leave the run done or refused".into(),
        ),
        Some(_) if !ends_done(steps, is_restore) => err(
            id,
            "an install or update must end with a wait_for helix_service step (or, for a panel \
             flashed over USB, its serial_flash step), so it is done only once HelixScreen runs"
                .into(),
        ),
        _ => {}
    }
    let commits: Vec<usize> = (0..steps.len()).filter(|&i| steps[i].commits()).collect();
    if commits.len() > 1 {
        err(id, "more than one step has commits: true".into());
    }
    if let Some(first) = first_change(steps) {
        match commits.first() {
            None => err(
                id,
                format!("step {first} changes the printer but no step has commits: true"),
            ),
            Some(&c) if c > first => err(
                id,
                format!(
                    "step {c} has commits: true after the first step that changes the printer \
                     (step {first})"
                ),
            ),
            _ => {}
        }
    }
    if let Some(&c) = commits.first() {
        stale_after_commit(id, steps, c, err);
        release_checked_before(id, steps, c, err);
    }
    let artifact = |name: &str, err: &mut dyn FnMut(&str, String)| {
        if !m.artifacts.contains_key(name) {
            err(id, format!("references unknown artifact {name:?}"));
        }
    };
    for (i, s) in steps.iter().enumerate() {
        match s {
            Step::Download { artifact: a, .. } => artifact(a, err),
            Step::WriteStick {
                artifacts, rename, ..
            } => {
                artifacts.iter().for_each(|a| artifact(a, err));
                rename.keys().for_each(|a| artifact(a, err));
            }
            Step::Info {
                image: Some(img), ..
            }
            | Step::UserAction {
                image: Some(img), ..
            } => check_image(id, img, asset_root, published, err),
            Step::Ask { choices, .. } => {
                for img in choices.iter().filter_map(|c| c.image.as_deref()) {
                    check_image(id, img, asset_root, published, err);
                }
            }
            Step::SshLogin { logins, .. }
                if logins
                    .iter()
                    .any(|l| l.passphrase.is_some() || l.sudo_password.is_some()) =>
            {
                err(
                    id,
                    format!(
                        "step {i}: a manifest login names no passphrase or sudo_password; \
                         those are typed by the user"
                    ),
                )
            }
            // An install erases a panel that does not run HelixScreen; the user hears it first.
            Step::SerialFlash { update: false, .. }
                if !steps[..i].iter().any(|s| matches!(s, Step::Info { .. })) =>
            {
                err(
                    id,
                    format!(
                        "step {i}: serial_flash erases a panel that does not run HelixScreen, so an \
                         info step before it must say so"
                    ),
                )
            }
            Step::Probe { .. } if s.probe().is_none() => err(
                id,
                format!("step {i}: probe needs exactly one of moonraker, ssh+fact, tcp_port+fact, serial"),
            ),
            _ => {}
        }
        if let Some(problem) = s.shape_error() {
            err(id, format!("step {i}: {problem}"));
        }
    }
}
