// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;
use std::io::Write;

use clap::ArgGroup;
use itertools::Itertools as _;
use jj_lib::file_util;
use jj_lib::repo::Repo;
use jj_lib::view::View;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use super::git;
use crate::cli_util::{user_error_with_hint, user_error_with_message, CommandError, CommandHelper};
use crate::ui::Ui;

/// Create a new repo in the given directory
///
/// If the given directory does not exist, it will be created. If no directory
/// is given, the current directory is used.
#[derive(clap::Args, Clone, Debug)]
#[command(group(ArgGroup::new("backend").args(&["git", "git_repo"])))]
pub(crate) struct InitArgs {
    /// The destination directory
    #[arg(default_value = ".", value_hint = clap::ValueHint::DirPath)]
    destination: String,
    /// Use the Git backend, creating a jj repo backed by a Git repo
    #[arg(long)]
    git: bool,
    /// Path to a git repo the jj repo will be backed by
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    git_repo: Option<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_init(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &InitArgs,
) -> Result<(), CommandError> {
    let cwd = command.cwd().canonicalize().unwrap();
    let wc_path = cwd.join(&args.destination);
    let wc_path = file_util::create_or_reuse_dir(&wc_path)
        .and_then(|_| wc_path.canonicalize())
        .map_err(|e| user_error_with_message("Failed to create workspace", e))?;
    let relative_wc_path = file_util::relative_path(&cwd, &wc_path);

    if let Some(git_store_str) = &args.git_repo {
        let git_store_path = cwd.join(git_store_str);
        let (workspace, repo) =
            Workspace::init_external_git(command.settings(), &wc_path, &git_store_path)?;
        let mut workspace_command = command.for_loaded_repo(ui, workspace, repo)?;
        git::maybe_add_gitignore(&workspace_command)?;
        // Import refs first so all the reachable commits are indexed in
        // chronological order.
        workspace_command.import_git_refs(ui)?;
        workspace_command.maybe_snapshot(ui)?;
        if !workspace_command.working_copy_shared_with_git() {
            let mut tx = workspace_command.start_transaction();
            jj_lib::git::import_head(tx.mut_repo())?;
            if let Some(git_head_id) = tx.mut_repo().view().git_head().as_normal().cloned() {
                let git_head_commit = tx.mut_repo().store().get_commit(&git_head_id)?;
                tx.check_out(&git_head_commit)?;
            }
            if tx.mut_repo().has_changes() {
                tx.finish(ui, "import git head")?;
            }
        }
        print_trackable_remote_branches(ui, workspace_command.repo().view())?;
    } else if args.git {
        if wc_path.join(".git").exists() {
            return Err(user_error_with_hint(
                "Did not create a jj repo because there is an existing Git repo in this directory.",
                format!(
                    r#"To create a repo backed by the existing Git repo, run `jj init --git-repo={}` instead."#,
                    relative_wc_path.display()
                ),
            ));
        }

        Workspace::init_internal_git(command.settings(), &wc_path)?;
    } else {
        if !command.settings().allow_native_backend() {
            return Err(user_error_with_hint(
                "The native backend is disallowed by default.",
                "Did you mean to pass `--git`?
Set `ui.allow-init-native` to allow initializing a repo with the native backend.",
            ));
        }
        Workspace::init_local(command.settings(), &wc_path)?;
    };

    writeln!(
        ui.stderr(),
        "Initialized repo in \"{}\"",
        relative_wc_path.display()
    )?;
    Ok(())
}

fn print_trackable_remote_branches(ui: &Ui, view: &View) -> io::Result<()> {
    let remote_branch_names = view
        .branches()
        .filter(|(_, branch_target)| branch_target.local_target.is_present())
        .flat_map(|(name, branch_target)| {
            branch_target
                .remote_refs
                .into_iter()
                .filter(|&(_, remote_ref)| !remote_ref.is_tracking())
                .map(move |(remote, _)| format!("{name}@{remote}"))
        })
        .collect_vec();
    if remote_branch_names.is_empty() {
        return Ok(());
    }

    writeln!(
        ui.hint(),
        "The following remote branches aren't associated with the existing local branches:"
    )?;
    let mut formatter = ui.stderr_formatter();
    for full_name in &remote_branch_names {
        write!(formatter, "  ")?;
        writeln!(formatter.labeled("branch"), "{full_name}")?;
    }
    drop(formatter);
    writeln!(
        ui.hint(),
        "Hint: Run `jj branch track {names}` to keep local branches updated on future pulls.",
        names = remote_branch_names.join(" "),
    )?;
    Ok(())
}
