//! Git repository operations used by the workflows.
//!
//! Local operations (opening, branch creation, staging, committing) use
//! `git2` (libgit2). Network operations — clone and push — shell out to the
//! system `git` binary, which on this platform provides a TLS backend that
//! libgit2 may lack. Authentication uses any configured git credential helper
//! (e.g. `gh auth git-credential`), or an explicit token via `Authorization`
//! headers.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use git2::{build::CheckoutBuilder, Oid, Signature};

/// An error from a git operation.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not inside a git repository")]
    NotARepository,
    #[error("path `{0}` is outside the repository")]
    OutsideRepo(String),
    #[error("git command failed: {command} {stderr}")]
    CommandFailed { command: String, stderr: String },
}

/// A convenience handle to an open git repository.
pub struct Repo {
    repo: git2::Repository,
    pub path: PathBuf,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo").field("path", &self.path).finish()
    }
}

impl Repo {
    /// Open an existing repository at `path`.
    pub fn open(path: &Path) -> Result<Repo, RepoError> {
        let repo = git2::Repository::open(path)?;
        Ok(Repo {
            repo,
            path: path.to_path_buf(),
        })
    }

    /// Build a `git` command, optionally injecting a PAT via an `Authorization`
    /// header so pushes/clones authenticate without a configured helper.
    fn git_command(token: Option<&str>) -> Command {
        let mut cmd = Command::new("git");
        if let Some(tok) = token {
            // GitHub accepts any username with a token as the password;
            // `x-access-token` is the convention.
            let b64 =
                base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{tok}"));
            cmd.arg("-c")
                .arg(format!("http.extraheader=AUTHORIZATION: basic {b64}"))
                .arg("-c")
                .arg("credential.helper=");
        }
        cmd
    }

    fn run_git(cmd: &mut Command) -> Result<(), RepoError> {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let output = cmd.output()?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let advanced = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Err(RepoError::CommandFailed {
                command: format!("{cmd:?}"),
                stderr: format!("{stderr}\n{advanced}"),
            })
        }
    }

    /// Clone a remote repository into `path` using the system `git`.
    pub fn clone(url: &str, path: &Path, token: Option<&str>) -> Result<Repo, RepoError> {
        let mut cmd = Self::git_command(token);
        cmd.arg("clone").arg(url).arg(path);
        Self::run_git(&mut cmd)?;
        Self::open(path)
    }

    /// The current branch name, if on a branch.
    pub fn current_branch(&self) -> Result<Option<String>, RepoError> {
        let head = self.repo.head()?;
        if head.is_branch() {
            let shorthand = head.shorthand()?.to_string();
            Ok(Some(shorthand))
        } else {
            Ok(None)
        }
    }

    /// Ensure the given branch exists (creating it if needed), split off from
    /// the current HEAD, and check it out.
    pub fn checkout_branch(&mut self, name: &str) -> Result<(), RepoError> {
        let current = self.repo.head()?.peel_to_commit()?;

        let branch = self.repo.branch(name, &current, false);
        let branch_ref = match branch {
            Ok(b) => b,
            Err(e) if e.code() == git2::ErrorCode::Exists => {
                self.repo.find_branch(name, git2::BranchType::Local)?
            }
            Err(e) => return Err(RepoError::Git(e)),
        };

        let obj = branch_ref.get().peel_to_commit()?;
        let commit = obj.as_object();
        self.repo.set_head(&format!("refs/heads/{name}"))?;
        self.repo
            .checkout_tree(commit, Some(CheckoutBuilder::new().safe()))?;
        Ok(())
    }

    /// Stage a file (or directory) path, optionally deleting it. Paths are
    /// interpreted relative to the repo root. `delete` removes it from the index.
    pub fn add_path(&self, rel_path: &str, delete: bool) -> Result<(), RepoError> {
        let rel = Path::new(rel_path);
        let full = self.path.join(rel);
        if !is_within(&full, &self.path) {
            return Err(RepoError::OutsideRepo(rel_path.to_string()));
        }

        if delete {
            let mut idx = self.repo.index()?;
            idx.remove_path(rel).map_err(RepoError::Git)?;
            idx.write()?;
        } else {
            let mut idx = self.repo.index()?;
            idx.add_path(rel).map_err(RepoError::Git)?;
            idx.write()?;
        }
        Ok(())
    }

    /// Stage all `paths` (relative to repo root). Optionally delete.
    pub fn add_paths(&self, paths: &[(String, bool)]) -> Result<(), RepoError> {
        for (p, del) in paths {
            self.add_path(p, *del)?;
        }
        Ok(())
    }

    /// Create a commit with the given author/signature and message. Returns the
    /// new commit id.
    pub fn commit(
        &self,
        author_name: &str,
        author_email: &str,
        message: &str,
    ) -> Result<Oid, RepoError> {
        let mut idx = self.repo.index()?;
        let tree_id = idx.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let sig = Signature::now(author_name, author_email)?;

        let parent = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parent_ids: Vec<&git2::Commit> = parent.iter().collect();

        let oid = if parent_ids.is_empty() {
            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
        } else {
            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_ids)
        }?;
        Ok(oid)
    }

    /// Push the current HEAD to `remote`/`branch`. `token` may be embedded in
    /// the remote URL for authenticated pushes; if None the configured remote
    /// is used as-is.
    /// Push the current HEAD to `remote`/`branch`. Authentication comes from a
    /// configured credential helper, or from `token` if provided.
    pub fn push(
        &self,
        remote_name: &str,
        branch: &str,
        token: Option<&str>,
    ) -> Result<(), RepoError> {
        let mut cmd = Self::git_command(token);
        cmd.arg("push");
        cmd.arg("--set-upstream");
        cmd.arg(remote_name);
        cmd.arg(branch);
        cmd.current_dir(&self.path);
        Self::run_git(&mut cmd)
    }

    /// Add a file from a byte buffer into the working tree at `rel_path`.
    pub fn write_file(&self, rel_path: &str, content: &str) -> Result<(), RepoError> {
        let full = self.path.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, content)?;
        Ok(())
    }

    /// Read a file from the working tree at `rel_path`.
    pub fn read_file(&self, rel_path: &str) -> Result<String, RepoError> {
        let full = self.path.join(rel_path);
        Ok(std::fs::read_to_string(full)?)
    }

    /// Check whether a file exists in the working tree at `rel_path`.
    pub fn file_exists(&self, rel_path: &str) -> bool {
        self.path.join(rel_path).is_file()
    }

    /// Write a blob to the repository and return its oid, without touching the
    /// working tree. Used for low-level tree construction.
    pub fn write_blob(&self, content: &str) -> Result<Oid, RepoError> {
        let blob = self.repo.blob(content.as_bytes())?;
        Ok(blob)
    }
}

/// Returns true if `path` lexically stays within `root` (no `..` escapes).
fn is_within(path: &Path, root: &Path) -> bool {
    // Canonicalize both; if the path doesn't exist yet, canonicalize its parent.
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canonical_path = match path.canonicalize() {
        Ok(p) => p,
        // File may not exist yet; canonicalize the nearest existing ancestor
        // and re-append the remaining components.
        Err(_) => {
            let mut anc = path.to_path_buf();
            let mut suffix = Vec::new();
            loop {
                if anc.canonicalize().is_ok() || anc == root {
                    break;
                }
                match anc.file_name() {
                    Some(n) => suffix.push(n.to_owned()),
                    None => break,
                }
                if !anc.pop() {
                    break;
                }
            }
            let base = anc.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let mut full = base;
            for comp in suffix.iter().rev() {
                full.push(comp);
            }
            full
        }
    };
    canonical_path.starts_with(&canonical_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let mut r = Repo {
            repo,
            path: dir.path().to_path_buf(),
        };
        r.write_file("file.txt", "hello").unwrap();
        r.add_path("file.txt", false).unwrap();
        let oid = r.commit("Test", "test@example.com", "initial").unwrap();
        assert!(!oid.is_zero());
        // The branch checkout should work with no commits? Guard: repo has a
        // commit now.
        r.checkout_branch("feature").unwrap();
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("feature"));
    }

    #[test]
    fn parse_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let r = Repo {
            repo,
            path: dir.path().to_path_buf(),
        };
        let err = r.add_path("../evil.txt", false).unwrap_err();
        assert!(matches!(err, RepoError::OutsideRepo(_)));
    }
}
